//! Scheduler / run-queue traits + a default round-robin implementation.
//!
//! [`RoundRobinScheduler`] is a minimal placeholder; the production
//! scheduler in `core::scheduler` drives execution. The trait surface
//! is kept intentionally narrow so a concrete scheduler can be swapped
//! in (or moved out of OSTD entirely) without touching consumers.

use crate::mm::{KArc, KVec, KVecDeque};
use crate::sync::LOCK_LEVEL_SCHEDULER;
use crate::sync::SpinLock;
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

// ---------------------------------------------------------------------------
// RoundRobinScheduler — minimal default impl.
// ---------------------------------------------------------------------------

/// Per-CPU run queue used by [`RoundRobinScheduler`].
pub struct RoundRobinRq {
    queue: KVecDeque<TaskRef>,
    current: Option<TaskRef>,
}

impl RoundRobinRq {
    /// Construct an empty run queue.
    pub fn new() -> Self {
        Self {
            queue: KVecDeque::new(),
            current: None,
        }
    }
}

impl Default for RoundRobinRq {
    fn default() -> Self {
        Self::new()
    }
}

impl RunQueue for RoundRobinRq {
    fn update_curr(&mut self) {
        // No-op for the placeholder. A real scheduler would account
        // vruntime here.
    }

    fn pick_next(&mut self) -> Option<TaskRef> {
        // If there is a current task, push it back on the queue
        // (round-robin) before picking the next.
        if let Some(prev) = self.current.take() {
            // KVecDeque::push_back returns Result<_, AllocError>;
            // ignore failure here (a placeholder scheduler trips OOM
            // in fully degraded conditions).
            let _ = self.queue.push_back(prev);
        }
        let next = self.queue.pop_front();
        self.current = next.clone();
        next
    }

    fn dequeue_curr(&mut self) -> Option<TaskRef> {
        self.current.take()
    }
}

/// Minimal default scheduler shipped inside OSTD.
///
/// One per-CPU run queue, FIFO ordering. Does not interact with the
/// per-CPU PCR — `enqueue` / `local_rq_with` operate on the
/// configured queue index passed in via [`RoundRobinScheduler::with_capacity`].
///
/// The kernel scheduler in `core::scheduler` is the real production
/// scheduler; this default impl exists so the trait surface is
/// non-empty at the OSTD layer.
pub struct RoundRobinScheduler {
    runqueues: KVec<SpinLock<RoundRobinRq>>,
    /// Index used by `local_rq_with` and `enqueue` until per-CPU
    /// dispatch is wired. Defaults to 0.
    default_queue: core::sync::atomic::AtomicUsize,
}

impl RoundRobinScheduler {
    /// Create a scheduler with `cpu_count` per-CPU run queues.
    pub fn with_capacity(cpu_count: usize) -> Result<Self, crate::mm::AllocError> {
        let mut runqueues = KVec::with_capacity(cpu_count)?;
        for _ in 0..cpu_count {
            runqueues.push(SpinLock::new(RoundRobinRq::new(), LOCK_LEVEL_SCHEDULER))?;
        }
        Ok(Self {
            runqueues,
            default_queue: core::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Override which queue `local_rq_with`/`enqueue` operate on.
    /// Production callers register a per-CPU backend that picks the
    /// real CPU index; the default index is purely for unit tests.
    pub fn set_default_queue(&self, idx: usize) {
        self.default_queue
            .store(idx, core::sync::atomic::Ordering::Relaxed);
    }

    fn pick_queue(&self) -> &SpinLock<RoundRobinRq> {
        let idx = self
            .default_queue
            .load(core::sync::atomic::Ordering::Relaxed);
        &self.runqueues[idx % self.runqueues.len()]
    }
}

impl Scheduler for RoundRobinScheduler {
    fn enqueue(&self, task: TaskRef) {
        let mut rq = self.pick_queue().lock();
        let _ = rq.queue.push_back(task);
    }

    fn local_rq_with(&self, f: &mut dyn FnMut(&mut dyn RunQueue)) {
        let mut rq = self.pick_queue().lock();
        f(&mut *rq as &mut dyn RunQueue);
    }
}
