//! Ownership of a half-built task for the length of its build.
//!
//! A [`PendingTask`] is deliberately unregistered — no lookup, active-task
//! walk, census or shutdown sweep can see it — so nothing observes the task
//! half-built, and equally nothing can recover the orphan if the token is
//! dropped on the floor. [`SpawnGuard`]'s [`Drop`] is what releases the child
//! on every exit from the build, including a kill.

use slopos_ostd::cpu::preempt::PreemptGuard;

use super::Task;
use super::task_lifecycle::task_abandon;
use super::task_table::{PendingTask, TaskRef};

/// Owns a half-built task for the whole construction window.
pub struct SpawnGuard {
    child_id: u32,
    child_process_id: u32,
    /// Captured rather than resolved on demand: a failed spawn can return
    /// `child_process_id` to the allocator, and the designator fails closed
    /// where a re-resolved number would name a stranger.
    child_table: Option<slopos_fs::fileio::FdTable>,
    /// `None` once [`commit`](Self::commit) has taken it.
    pending: Option<PendingTask>,
}

impl SpawnGuard {
    pub fn new(mut pending: PendingTask) -> Self {
        let child_id = pending.id();
        let (child_process_id, child_table) = {
            let task = pending.as_mut();
            let table = task
                .process()
                .as_deref()
                .and_then(slopos_fs::fileio::FdTable::of);
            (task.process_id, table)
        };
        Self {
            child_id,
            child_process_id,
            child_table,
            pending: Some(pending),
        }
    }

    #[inline]
    pub fn child_id(&self) -> u32 {
        self.child_id
    }

    #[inline]
    pub fn child_process_id(&self) -> u32 {
        self.child_process_id
    }

    /// The child's descriptor table. `None` for a kernel task.
    #[inline]
    pub fn child_table(&self) -> Option<slopos_fs::fileio::FdTable> {
        self.child_table
    }

    /// Exclusive access to the child under construction; `None` once the guard
    /// is spent.
    ///
    /// `f` may allocate and take locks but must not block or yield. Anything
    /// whose release can deallocate — a displaced `KArc` — must be returned out
    /// of `f` and dropped by the caller: the buddy allocator's reuse path
    /// performs synchronous cross-CPU TLB drains, which the preempt guard held
    /// here forbids.
    pub fn with_child<R>(&mut self, f: impl FnOnce(&mut Task) -> R) -> Option<R> {
        let _preempt = PreemptGuard::new();
        let pending = self.pending.as_mut()?;
        Some(f(pending.as_mut()))
    }

    /// Make the child reachable.
    ///
    /// `None` means the registry was full or the guard was already spent;
    /// either way nothing is left to release.
    pub fn commit(mut self) -> Option<TaskRef> {
        let token = self.pending.take()?;
        let _preempt = PreemptGuard::new();
        super::task_lifecycle::task_commit(token)
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if let Some(token) = self.pending.take() {
            task_abandon(token);
        }
    }
}
