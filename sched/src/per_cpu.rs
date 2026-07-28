//! Per-CPU Scheduler for SMP Support
//!
//! Each CPU has its own scheduler instance with local run queues.
//! This minimizes lock contention and improves cache locality.
//!
//! # Safety Model
//!
//! `PriorityRunQueue` uses interior mutability throughout so that all public
//! APIs take `&self` (shared reference). This eliminates the UB that arose
//! from handing out `&mut` to a `static` array element from multiple CPUs.
//!
//! - Atomic fields: direct load/store (lock-free).
//! - `ready_queues`: backed by `IntrusiveLinkedList<Task>` per priority
//!   level; the list itself uses interior atomics, but operations are
//!   serialised by `queue_lock` since the linked-list primitive is not
//!   lock-free across operations.
//! - `return_context`: wrapped in `UnsafeCell`, only written during
//!   single-threaded init and read by the owning CPU.

use core::cell::SyncUnsafeCell;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Round-robin counter for fork/spawn CPU placement.  Rotates the starting
/// position in `find_idlest_cpu()` so sequential forks spread across CPUs
/// even when all are idle (all have the same load).  Mirrors Linux's
/// `for_each_cpu_wrap()` pattern in `sched_balance_find_dst_group_cpu()`.
static FORK_RR_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Test-hooks accessor: read the current `FORK_RR_COUNTER` value for
/// the hermetic-state snapshot.
#[cfg(feature = "test-hooks")]
pub fn fork_rr_counter_value() -> usize {
    FORK_RR_COUNTER.load(Ordering::Relaxed)
}

/// Test-hooks accessor: restore `FORK_RR_COUNTER` from a snapshot.
#[cfg(feature = "test-hooks")]
pub fn fork_rr_counter_set(value: usize) {
    FORK_RR_COUNTER.store(value, Ordering::Relaxed);
}

use super::task::{
    TaskRef, task_cpu_affinity, task_last_cpu, task_next_inbox_load, task_next_inbox_store_relaxed,
    task_next_inbox_store_release, task_put, task_remote_inbox_try_link, task_remote_inbox_unlink,
    task_sched_placement_compare_exchange, task_status,
};
use super::task_struct::{SwitchContext, Task};
use slopos_abi::task::TaskStatus;
use slopos_arch::MAX_CPUS;
use slopos_ostd::sync::intrusive::{IntrusiveLinkedList, LinkError};
use slopos_ostd::sync::{InitFlag, KernelSync, LOCK_LEVEL_SCHEDULER, SpinLock};
use slopos_ostd::task::{SchedPlacement, TaskAddr, task_placement_retain};
use slopos_ostd::{klog_debug, klog_info};

/// One slot per [`TaskPriority`] variant: `High`, `KernelIo`,
/// `Normal`, `Low`, `Idle`. Bumped from 4→5 when `KernelIo` landed
/// in Phase 1 of the scheduler refactor. The repr value of each
/// variant is the index into [`PriorityRunQueue::ready_queues`].
const NUM_PRIORITY_LEVELS: usize = 5;

/// Role tag for the per-CPU `ReadyQueue` intrusive list. Defined in
/// OSTD so the kernel `TaskInner<K, U>` can `impl LinkProvider` against
/// it without OSTD reaching into `core/`.
pub use slopos_ostd::task::link_roles::ReadyQueueRole;

/// Per-priority FIFO of ready tasks. Refcount accounting is the
/// caller's job (incremented on enqueue, decremented on dequeue /
/// remove / drain).
struct ReadyQueue {
    list: KernelSync<IntrusiveLinkedList<Task, ReadyQueueRole>>,
}

impl ReadyQueue {
    const fn new() -> Self {
        Self {
            list: KernelSync::new(IntrusiveLinkedList::new()),
        }
    }

    /// Drop every linked task, releasing each one's parked owning reference as
    /// we pop. Used during scheduler shutdown / per-CPU reinitialisation.
    fn clear_with_ref_release(&self) {
        while let Some(node) = self.list.pop() {
            let raw = node.as_ptr();
            let _ = task_sched_placement_compare_exchange(
                raw,
                SchedPlacement::ReadyQueue,
                SchedPlacement::None,
            );
            task_put(TaskRef::from_placement(node));
        }
    }

    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    fn len(&self) -> u32 {
        self.list.len() as u32
    }

    /// Link a task whose placement has already been transitioned to
    /// `SchedPlacement::ReadyQueue`.
    ///
    /// Takes the caller's handle rather than a pointer because membership
    /// *mints* a reference: `task_placement_retain` is sound exactly because
    /// the caller already holds a live one, which is what `&TaskRef` says.
    ///
    /// Returns:
    /// - `0` when the task was newly linked and its owning reference parked;
    /// - `1` when it was already linked in a ready queue;
    /// - `-1` on an unexpected list error.
    fn link_preclaimed_with_status(&self, task: &TaskRef) -> i32 {
        let node = task.node();
        match self.list.push(node) {
            Ok(()) => {
                // Membership now holds one owning reference to the task.
                task_placement_retain(node);
                0
            }
            Err(LinkError::AlreadyLinked) => 1,
            Err(LinkError::NotPresent) => -1,
        }
    }

    /// Detach the head task and hand the caller the owning reference this
    /// queue held.
    ///
    /// The membership reference is *moved* out rather than released: the
    /// dispatcher needs a reference for the whole window between dequeue and
    /// the switch, and releasing here would leave the task pinned by nothing
    /// across that window — including an unbounded `on_cpu` spin.
    fn dequeue(&self) -> Option<TaskRef> {
        let node = self.list.pop()?;
        let _ = task_sched_placement_compare_exchange(
            node.as_ptr(),
            SchedPlacement::ReadyQueue,
            SchedPlacement::OnCpu,
        );
        Some(TaskRef::from_placement(node))
    }

    /// Unlink a task and release the owning reference membership held.
    ///
    /// Borrows the task rather than taking a handle: removal *releases* the
    /// queue's own reference and mints nothing, so the caller only has to
    /// prove the task is there to be unlinked.
    fn remove(&self, task: &Task) -> i32 {
        // Search with the borrow, reclaim with what the list hands back. The
        // two addresses are equal but not interchangeable:
        // `task_placement_reclaim` walks backwards out of the task body into
        // the `KArc` header, which a pointer derived from a `&Task` has no
        // provenance over. The list's own link pointer came from
        // `KArc::node`, which does.
        let Ok(node) = self.list.remove(NonNull::from(task)) else {
            return -1;
        };
        let _ = task_sched_placement_compare_exchange(
            task,
            SchedPlacement::ReadyQueue,
            SchedPlacement::None,
        );
        task_put(TaskRef::from_placement(node));
        0
    }

    /// Detach the tail task for migration, handing the thief the owning
    /// reference this queue held.
    ///
    /// As with [`ReadyQueue::dequeue`] the reference moves rather than being
    /// released: a stolen task travels through the work-stealer, possibly
    /// bouncing back to the victim, before any queue re-parks a reference for
    /// it. Carrying the handle makes the `Migrating` window owned like every
    /// other placement instead of relying on an anchor elsewhere.
    fn steal_from_tail(&self) -> Option<TaskRef> {
        if self.list.len() <= 1 {
            return None;
        }
        let last = self.list.iter().last()?;
        let raw = last.as_ptr();
        if !task_sched_placement_compare_exchange(
            raw,
            SchedPlacement::ReadyQueue,
            SchedPlacement::Migrating,
        ) {
            return None;
        }
        if self.list.remove(last).is_err() {
            let _ = task_sched_placement_compare_exchange(
                raw,
                SchedPlacement::Migrating,
                SchedPlacement::ReadyQueue,
            );
            return None;
        }
        Some(TaskRef::from_placement(last))
    }
}

const EMPTY_QUEUE: ReadyQueue = ReadyQueue::new();

#[repr(C, align(64))]
pub struct PriorityRunQueue {
    /// Owning CPU id. Written once during `init`, read everywhere via
    /// the `cpu_id` accessor; backed by `AtomicUsize` so the read path
    /// stays in safe Rust without an `UnsafeCell` carve-out.
    cpu_id_atom: AtomicUsize,
    ready_queues: [ReadyQueue; NUM_PRIORITY_LEVELS],
    queue_lock: SpinLock<()>,
    pub enabled: AtomicBool,
    /// Default time slice in ticks. Same `init`-once / read-everywhere
    /// pattern as `cpu_id_atom`.
    time_slice_atom: AtomicU32,
    pub total_switches: AtomicU64,
    pub total_preemptions: AtomicU64,
    pub total_ticks: AtomicU64,
    pub idle_time: AtomicU64,
    pub total_yields: AtomicU64,
    pub schedule_calls: AtomicU32,
    initialized: AtomicBool,
    pub return_context: SyncUnsafeCell<SwitchContext>,
    executing_task: AtomicBool,
    remote_inbox_head: AtomicPtr<Task>,
    inbox_count: AtomicU32,
}

impl PriorityRunQueue {
    pub const fn new() -> Self {
        Self {
            cpu_id_atom: AtomicUsize::new(0),
            ready_queues: [EMPTY_QUEUE; NUM_PRIORITY_LEVELS],
            queue_lock: SpinLock::new((), LOCK_LEVEL_SCHEDULER),
            enabled: AtomicBool::new(false),
            time_slice_atom: AtomicU32::new(10),
            total_switches: AtomicU64::new(0),
            total_preemptions: AtomicU64::new(0),
            total_ticks: AtomicU64::new(0),
            idle_time: AtomicU64::new(0),
            total_yields: AtomicU64::new(0),
            schedule_calls: AtomicU32::new(0),
            initialized: AtomicBool::new(false),
            return_context: SyncUnsafeCell::new(SwitchContext::zero()),
            executing_task: AtomicBool::new(false),
            remote_inbox_head: AtomicPtr::new(ptr::null_mut()),
            inbox_count: AtomicU32::new(0),
        }
    }

    /// Owning CPU id, set once during `init`.
    #[inline]
    pub fn cpu_id(&self) -> usize {
        self.cpu_id_atom.load(Ordering::Relaxed)
    }

    pub fn set_executing_task(&self, executing: bool) {
        self.executing_task.store(executing, Ordering::SeqCst);
    }

    pub fn is_executing_task(&self) -> bool {
        self.executing_task.load(Ordering::SeqCst)
    }

    /// Initialise this CPU's scheduler. Idempotent re-init across
    /// test fixtures uses the same path; the only ordering contract
    /// is that callers run this on the owning CPU during scheduler
    /// bring-up before any task is enqueued onto it.
    pub fn init(&self, cpu_id: usize) {
        self.cpu_id_atom.store(cpu_id, Ordering::Relaxed);
        self.time_slice_atom.store(10, Ordering::Relaxed);
        for queue in &self.ready_queues {
            queue.clear_with_ref_release();
        }
        self.enabled.store(false, Ordering::Relaxed);
        self.total_switches.store(0, Ordering::Relaxed);
        self.total_preemptions.store(0, Ordering::Relaxed);
        self.total_ticks.store(0, Ordering::Relaxed);
        self.idle_time.store(0, Ordering::Relaxed);
        self.total_yields.store(0, Ordering::Relaxed);
        self.schedule_calls.store(0, Ordering::Relaxed);
        self.initialized.store(true, Ordering::Release);
        self.clear_remote_inbox_with_ref_release();
        self.inbox_count.store(0, Ordering::Relaxed);
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn enqueue_local(&self, task: &TaskRef) -> i32 {
        let from = if task.sched_placement() == SchedPlacement::Waking {
            SchedPlacement::Waking
        } else {
            SchedPlacement::None
        };
        match self.enqueue_local_from_placement(task, from) {
            0 | 1 => 0,
            _ => -1,
        }
    }

    /// Requeue a task that this scheduler already owns as current/dispatching.
    pub fn enqueue_from_on_cpu(&self, task: &TaskRef) -> i32 {
        match self.enqueue_local_from_placement(task, SchedPlacement::OnCpu) {
            0 | 1 => 0,
            _ => -1,
        }
    }

    /// Publish a task whose wake/new-task path reserved `SchedPlacement::Waking`.
    pub fn enqueue_waking(&self, task: &TaskRef) -> i32 {
        match self.enqueue_local_from_placement(task, SchedPlacement::Waking) {
            0 | 1 => 0,
            _ => -1,
        }
    }

    /// Park a migrating task on this CPU while the caller keeps the handle it
    /// carried across the `Migrating` window — the give-it-back paths, where
    /// the carried reference is released separately.
    pub fn enqueue_migrated_borrowed(&self, task: &TaskRef) -> i32 {
        match self.enqueue_local_from_placement(task, SchedPlacement::Migrating) {
            0 | 1 => 0,
            _ => -1,
        }
    }

    /// Park a migrating task on this CPU, consuming the handle the thief
    /// carried across the `Migrating` window.
    ///
    /// On failure the handle comes back in `Err` so the caller can return the
    /// task to its victim (or release it) rather than leaking the reference.
    pub fn enqueue_migrated(&self, task: TaskRef) -> Result<(), TaskRef> {
        match self.enqueue_local_from_placement(&task, SchedPlacement::Migrating) {
            // Membership parked its own reference, so the carried one is spent.
            0 | 1 => {
                super::task::task_put(task);
                Ok(())
            }
            _ => Err(task),
        }
    }

    /// Enqueue locally and preserve the inserted-vs-already-owned outcome.
    /// See [`ReadyQueue::link_preclaimed_with_status`] for return values.
    pub fn enqueue_local_with_status(&self, task: &TaskRef) -> i32 {
        self.enqueue_local_from_placement(task, SchedPlacement::None)
    }

    fn enqueue_local_from_placement(&self, task: &TaskRef, from: SchedPlacement) -> i32 {
        let body: &Task = task;

        let current = body.sched_placement();
        // A never-published task is not enqueueable by anyone but its creator,
        // and its creator goes through `publish_new_task`. `-1`, not `1`: `1`
        // means "some queue already owns it", which would make the publish
        // fallback believe the task landed somewhere.
        if current == SchedPlacement::Nascent {
            return -1;
        }
        if current == SchedPlacement::ReadyQueue || current == SchedPlacement::RemoteWake {
            return 1;
        }
        if current == SchedPlacement::OnCpu && from != SchedPlacement::OnCpu {
            return 1;
        }
        if current == SchedPlacement::Migrating && from != SchedPlacement::Migrating {
            return 1;
        }
        if current == SchedPlacement::Waking && from != SchedPlacement::Waking {
            return 1;
        }
        if current != from {
            return -1;
        }
        if !body.sched_placement_compare_exchange(from, SchedPlacement::ReadyQueue) {
            let after = body.sched_placement();
            if after != SchedPlacement::None {
                return 1;
            }
            return -1;
        }

        let result = self.enqueue_local_preclaimed(task);
        if result < 0 {
            let _ = body.sched_placement_compare_exchange(SchedPlacement::ReadyQueue, from);
        }
        result
    }

    fn enqueue_local_preclaimed(&self, task: &TaskRef) -> i32 {
        let self_addr = self as *const _ as usize;
        if self_addr < 0xffffffff80000000 {
            klog_info!(
                "SCHED: BUG - enqueue_local called with invalid self=0x{:x}",
                self_addr
            );
            return -1;
        }
        let body: &Task = task;
        let priority = body.priority;
        let idx = (priority as usize).min(NUM_PRIORITY_LEVELS - 1);

        body.set_last_cpu(self.cpu_id() as u8);

        let _guard = self.queue_lock.lock();
        self.ready_queues[idx].link_preclaimed_with_status(task)
    }

    pub fn dequeue_highest_priority(&self) -> Option<TaskRef> {
        let self_addr = self as *const _ as usize;
        if self_addr < 0xffffffff80000000 {
            klog_info!(
                "SCHED: BUG - dequeue_highest_priority called with invalid self=0x{:x}",
                self_addr
            );
            return None;
        }
        let _guard = self.queue_lock.lock();
        for queue in &self.ready_queues {
            if let Some(task) = queue.dequeue() {
                return Some(task);
            }
        }
        None
    }

    pub fn remove_task(&self, task: &Task) -> i32 {
        let priority = task.priority;
        let idx = (priority as usize).min(NUM_PRIORITY_LEVELS - 1);
        let _guard = self.queue_lock.lock();
        self.ready_queues[idx].remove(task)
    }

    pub fn total_ready_count(&self) -> u32 {
        let _guard = self.queue_lock.lock();
        self.ready_queues.iter().map(|q| q.len()).sum()
    }

    /// Pending cross-core wakes parked in this CPU's remote inbox. Like
    /// [`Self::effective_load`], treat a non-null head as at least one entry:
    /// `push_remote_wake` links the head before bumping `inbox_count`, so a
    /// bare count can momentarily undercount. Used by `resume_all_aps` to wake a
    /// paused AP that has an inbox wake but no ready-queue entry.
    pub fn inbox_count(&self) -> u32 {
        let inbox = self.inbox_count.load(Ordering::Relaxed);
        if inbox == 0 && !self.remote_inbox_head.load(Ordering::Acquire).is_null() {
            1
        } else {
            inbox
        }
    }

    /// Returns the effective load on this CPU: queued tasks plus one if a
    /// non-idle task is currently running.  Lock-free and approximate.
    /// Mirrors Linux's `rq->nr_running` which includes the running task.
    pub fn effective_load(&self) -> u32 {
        let queued: u32 = self.ready_queues.iter().map(|q| q.len()).sum();
        let inbox = self.inbox_count.load(Ordering::Relaxed);
        // push_remote_wake() links into remote_inbox_head BEFORE
        // incrementing inbox_count, so treat a non-null head as at
        // least one pending task to avoid undercounting.
        let inbox = if inbox == 0 && !self.remote_inbox_head.load(Ordering::Acquire).is_null() {
            1
        } else {
            inbox
        };
        let cpu_id = self.cpu_id();
        // The bootstrap check still reads the slot raw: a stub is eight bytes
        // and has no task identity to compare, so it is recognised by address
        // range. Whether the CPU is on its *idle* task is pure identity, and
        // that is what `TaskAddr` exists for.
        let raw_current = slopos_arch::pcr::get_current_task_for(cpu_id);
        let current = TaskAddr::current_of(cpu_id);
        let running_real = current.is_some()
            && !crate::safestack_rt::is_bootstrap_task_ptr(raw_current.cast_const())
            && current != TaskAddr::idle_of(cpu_id);
        let load = queued.saturating_add(inbox);
        if running_real {
            load.saturating_add(1)
        } else {
            load
        }
    }

    /// Reset inbox_count to zero.  For test fixtures only — clears stale
    /// counts that leak between test runs due to SMP timing.
    #[cfg(feature = "test-hooks")]
    pub fn force_clear_inbox_count(&self) {
        self.clear_remote_inbox_with_ref_release();
        self.inbox_count.store(0, Ordering::Relaxed);
    }

    pub fn steal_task(&self) -> Option<TaskRef> {
        let _guard = self.queue_lock.lock();
        for queue in self.ready_queues.iter().rev() {
            if let Some(task) = queue.steal_from_tail() {
                return Some(task);
            }
        }
        None
    }

    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn increment_switches(&self) {
        self.total_switches.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_preemptions(&self) {
        self.total_preemptions.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_ticks(&self) {
        self.total_ticks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_idle_time(&self) {
        self.idle_time.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_yields(&self) {
        self.total_yields.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_schedule_calls(&self) {
        self.schedule_calls.fetch_add(1, Ordering::Relaxed);
    }

    /// Push a task to this CPU's remote wake inbox.
    ///
    /// This is a lock-free MPSC (multi-producer single-consumer) push.
    /// Can be called from ANY CPU safely.
    pub fn push_remote_wake(&self, task: &TaskRef) {
        let from = if task.sched_placement() == SchedPlacement::Waking {
            SchedPlacement::Waking
        } else {
            SchedPlacement::None
        };
        let _ = self.push_remote_wake_from_placement(task, from);
    }

    /// Publish a task whose wake/new-task path reserved `SchedPlacement::Waking`
    /// into this CPU's remote wake inbox.
    pub fn push_remote_wake_waking(&self, task: &TaskRef) -> i32 {
        self.push_remote_wake_from_placement(task, SchedPlacement::Waking)
    }

    fn push_remote_wake_from_placement(&self, task: &TaskRef, from: SchedPlacement) -> i32 {
        let node = task.node();
        let body: &Task = task;

        // Cross-role runnable ownership comes before the intrusive link. A
        // task already in a local ready queue, a remote inbox, an on-CPU
        // switch window, or a migration handoff is already scheduler-owned;
        // duplicate wakes are no-ops. This is the single gate that prevents
        // the historical ready-queue + remote-inbox double-placement race.
        let current = body.sched_placement();
        if current == SchedPlacement::ReadyQueue
            || current == SchedPlacement::RemoteWake
            || current == SchedPlacement::OnCpu
            || current == SchedPlacement::Migrating
            || (current == SchedPlacement::Waking && from != SchedPlacement::Waking)
        {
            return 1;
        }
        if current != from {
            return -1;
        }
        if !body.sched_placement_compare_exchange(from, SchedPlacement::RemoteWake) {
            let after = body.sched_placement();
            if after != SchedPlacement::None {
                return 1;
            }
            return -1;
        }

        // Acquire single-membership before publishing task into the lock-free
        // stack. The inbox uses the task's role-typed RemoteWakeRole Link: a
        // tail node has a null successor while still queued, so membership must
        // be tracked by the link's `linked` bit just like the ready queue.
        if !task_remote_inbox_try_link(body) {
            let _ = body.sched_placement_compare_exchange(SchedPlacement::RemoteWake, from);
            return -1;
        }

        // Park the inbox's owning reference before publishing the node so a
        // drain that immediately swaps the head cannot drop the last reference
        // before the producer has finished linking it.
        body.set_last_cpu(self.cpu_id() as u8);
        task_placement_retain(node);

        // Lock-free push using CAS loop (Treiber stack pattern)
        loop {
            // Load current head
            let old_head = self.remote_inbox_head.load(Ordering::Acquire);

            // Point our RemoteWakeRole link to the current head. `node` is a
            // non-null `*mut Task` pinned by the inbox reference above.
            task_next_inbox_store_relaxed(node.as_ptr(), old_head);

            // Try to become new head
            match self.remote_inbox_head.compare_exchange_weak(
                old_head,
                node.as_ptr(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Success! Update count and return
                    self.inbox_count.fetch_add(1, Ordering::Relaxed);
                    return 0;
                }
                Err(_) => {
                    // Lost race - retry
                    core::hint::spin_loop();
                }
            }
        }
    }

    /// Reverse the detached Treiber chain so the drain runs it in FIFO order.
    ///
    /// Walks the raw slot pointers and rewrites only each node's inbox link.
    /// That field *is* the placement slot, the one raw window invariant I1
    /// sanctions, and every node on the chain is still backed by the owning
    /// reference its producer parked — so the walk touches nothing it does not
    /// already own. Returns the new head and the node count.
    fn reverse_detached_inbox(head: *mut Task) -> (*mut Task, u32) {
        let mut reversed: *mut Task = ptr::null_mut();
        let mut cursor = head;
        let mut count = 0u32;
        while !cursor.is_null() {
            let next = task_next_inbox_load(cursor);
            task_next_inbox_store_relaxed(cursor, reversed);
            reversed = cursor;
            cursor = next;
            count = count.saturating_add(1);
        }
        (reversed, count)
    }

    /// Drain all tasks from remote inbox into local ready queues.
    /// MUST only be called by the owning CPU.
    ///
    /// Each node's parked reference is *reclaimed first*, so every status read,
    /// placement CAS and enqueue below is made through a handle this CPU owns.
    /// The reclaim is refcount-neutral — it is the inverse of the producer's
    /// park, not a second reference — so the drain still allocates nothing and
    /// spends no extra atomic per task.
    pub fn drain_remote_inbox(&self) {
        let head = self
            .remote_inbox_head
            .swap(ptr::null_mut(), Ordering::AcqRel);

        if head.is_null() {
            return;
        }

        let (mut cursor, count) = Self::reverse_detached_inbox(head);

        while let Some(node) = NonNull::new(cursor) {
            // Take the reference back before reading anything through it.
            let task = TaskRef::from_placement(node);
            let body: &Task = &task;
            let next = task_next_inbox_load(body);
            task_next_inbox_store_release(body, ptr::null_mut());

            if task_status(body) == Some(TaskStatus::Ready)
                && body.sched_placement_compare_exchange(
                    SchedPlacement::RemoteWake,
                    SchedPlacement::ReadyQueue,
                )
            {
                // The inbox owner transfers placement directly to its local
                // ready queue. During this short handoff the task is
                // scheduler-owned as ReadyQueue even before `ready_link` is
                // linked, so a duplicate wake cannot publish a second entry.
                task_remote_inbox_unlink(body);
                if self.enqueue_local_preclaimed(&task) < 0 {
                    let _ = body.sched_placement_compare_exchange(
                        SchedPlacement::ReadyQueue,
                        SchedPlacement::None,
                    );
                }
            } else {
                // The task is no longer Ready, or another owner repaired an
                // inconsistent placement. Drop the remote-inbox claim, then
                // re-check state. If a wake raced while producers observed
                // `RemoteWake` and therefore no-op'd, this CPU performs the
                // enqueue now; if the producer already enqueued after the
                // release, `enqueue_local` sees non-None placement and no-ops.
                task_remote_inbox_unlink(body);
                let _ = body.sched_placement_compare_exchange(
                    SchedPlacement::RemoteWake,
                    SchedPlacement::None,
                );
                if task_status(body) == Some(TaskStatus::Ready) {
                    let _ = self.enqueue_local(&task);
                }
            }

            // The drained reference goes last: releasing it may retire a
            // terminated task, and `body` must not outlive that.
            task_put(task);
            cursor = next;
        }

        self.saturating_sub_inbox_count(count);
    }

    /// Discard the whole inbox, releasing each parked reference. Same
    /// reclaim-then-read shape as [`Self::drain_remote_inbox`].
    fn clear_remote_inbox_with_ref_release(&self) {
        let mut cursor = self
            .remote_inbox_head
            .swap(ptr::null_mut(), Ordering::AcqRel);
        let mut drained = 0u32;
        while let Some(node) = NonNull::new(cursor) {
            let task = TaskRef::from_placement(node);
            let body: &Task = &task;
            let next = task_next_inbox_load(body);
            task_next_inbox_store_release(body, ptr::null_mut());
            task_remote_inbox_unlink(body);
            let _ = body
                .sched_placement_compare_exchange(SchedPlacement::RemoteWake, SchedPlacement::None);
            task_put(task);
            cursor = next;
            drained = drained.saturating_add(1);
        }
        self.saturating_sub_inbox_count(drained);
    }

    fn saturating_sub_inbox_count(&self, amount: u32) {
        if amount == 0 {
            return;
        }

        let mut current = self.inbox_count.load(Ordering::Acquire);
        loop {
            let next = current.saturating_sub(amount);
            match self.inbox_count.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Check if inbox has pending tasks
    #[inline]
    pub fn has_pending_inbox(&self) -> bool {
        !self.remote_inbox_head.load(Ordering::Acquire).is_null()
    }
}

use slopos_ostd::sync::CacheAligned;
use slopos_ostd::sync::cpu_local::CpuLocal;

/// The global preemptive priority scheduler. Owns one
/// [`PriorityRunQueue`] per CPU through [`CpuLocal`], which guarantees
/// per-slot pinning and cache-line alignment. The preemptive surface
/// (block, unblock, sleep, `schedule_task`, …) lives as free functions
/// in [`crate::scheduler`] and operates on raw `*mut Task` with manual
/// refcount accounting.
pub struct PriorityScheduler {
    runqueues: CpuLocal<PriorityRunQueue>,
    pub enabled: AtomicBool,
}

const PRIORITY_RQ_INIT: CacheAligned<PriorityRunQueue> = CacheAligned(PriorityRunQueue::new());

impl PriorityScheduler {
    pub const fn new() -> Self {
        Self {
            runqueues: CpuLocal::new_with([PRIORITY_RQ_INIT; MAX_CPUS]),
            enabled: AtomicBool::new(false),
        }
    }

    /// Borrow the per-CPU [`PriorityRunQueue`] for `cpu_id`. Returns
    /// `None` if `cpu_id` is out of range. Cross-CPU reads are valid
    /// because every interior field is atomic / `SpinLock`-protected.
    #[inline]
    pub fn runqueue_for(&'static self, cpu_id: usize) -> Option<&'static PriorityRunQueue> {
        self.runqueues.snapshot_for_cpu(cpu_id)
    }
}

/// The global preemptive scheduler instance.
pub static PRIORITY_SCHEDULER: PriorityScheduler = PriorityScheduler::new();

/// Bounds-checked accessor over the per-CPU run queues. Thin
/// delegate to [`PriorityScheduler::runqueue_for`].
#[inline]
fn cpu_scheduler(cpu_id: usize) -> Option<&'static PriorityRunQueue> {
    PRIORITY_SCHEDULER.runqueue_for(cpu_id)
}

/// `init_all_percpu_schedulers` init-once gate. `pub(crate)` so the
/// `test_hermetic::SchedulersInitFlag` HermeticState impl can
/// snapshot/restore it.
pub(crate) static SCHEDULERS_INIT: InitFlag = InitFlag::new();

pub fn init_percpu_scheduler(cpu_id: usize) {
    let Some(sched) = cpu_scheduler(cpu_id) else {
        return;
    };
    sched.init(cpu_id);
    klog_debug!("SCHED: Per-CPU scheduler initialized for CPU {}", cpu_id);
}

pub fn init_all_percpu_schedulers() {
    if !SCHEDULERS_INIT.init_once() {
        return;
    }

    for cpu_id in 0..MAX_CPUS {
        if let Some(sched) = cpu_scheduler(cpu_id) {
            sched.init(cpu_id);
        }
    }
}

pub fn is_percpu_scheduler_initialized(cpu_id: usize) -> bool {
    cpu_scheduler(cpu_id)
        .map(|s| s.is_initialized())
        .unwrap_or(false)
}

pub fn with_local_scheduler<R>(f: impl FnOnce(&PriorityRunQueue) -> R) -> R {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    let sched = cpu_scheduler(cpu_id).expect("get_current_cpu() returned an out-of-range CPU id");
    f(sched)
}

pub fn with_cpu_scheduler<R>(cpu_id: usize, f: impl FnOnce(&PriorityRunQueue) -> R) -> Option<R> {
    let sched = cpu_scheduler(cpu_id)?;
    if !sched.is_initialized() {
        return None;
    }
    Some(f(sched))
}

pub fn enqueue_task_on_cpu(cpu_id: usize, task: &TaskRef) -> i32 {
    if cpu_id >= MAX_CPUS {
        return -1;
    }

    let body: &Task = task;
    if task_status(body) != Some(TaskStatus::Ready) {
        return -1;
    }

    with_cpu_scheduler(cpu_id, |sched| match body.sched_placement() {
        // A borrowed re-enqueue of a migrating task (the give-it-back path)
        // parks a fresh membership reference; the caller still owns the handle
        // it carried and releases it separately.
        SchedPlacement::Migrating => sched.enqueue_migrated_borrowed(task),
        SchedPlacement::Waking => sched.enqueue_waking(task),
        _ => sched.enqueue_local(task),
    })
    .unwrap_or(-1)
}

pub fn try_steal_task_from_cpu(cpu_id: usize) -> Option<TaskRef> {
    with_cpu_scheduler(cpu_id, |sched| {
        if sched.total_ready_count() <= 1 {
            return None;
        }
        sched.steal_task()
    })
    .flatten()
}

pub fn get_cpu_ready_count(cpu_id: usize) -> u32 {
    with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0)
}

pub fn get_total_ready_tasks() -> u32 {
    let mut total = 0u32;
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        if let Some(count) = with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()) {
            total += count;
        }
    }
    total
}

pub fn get_total_switches() -> u64 {
    let mut total = 0u64;
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        if let Some(count) =
            with_cpu_scheduler(cpu_id, |sched| sched.total_switches.load(Ordering::Relaxed))
        {
            total = total.saturating_add(count);
        }
    }
    total
}

pub fn get_total_yields() -> u64 {
    let mut total = 0u64;
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        if let Some(count) =
            with_cpu_scheduler(cpu_id, |sched| sched.total_yields.load(Ordering::Relaxed))
        {
            total = total.saturating_add(count);
        }
    }
    total
}

pub fn get_total_schedule_calls() -> u32 {
    let mut total = 0u32;
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        if let Some(count) =
            with_cpu_scheduler(cpu_id, |sched| sched.schedule_calls.load(Ordering::Relaxed))
        {
            total = total.saturating_add(count);
        }
    }
    total
}

/// Check whether a CPU is genuinely idle: no queued tasks AND no real
/// (non-idle) task currently running.  Mirrors Linux's `idle_cpu()` which
/// checks `rq->nr_running == 0` (their nr_running includes the running task).
fn cpu_is_idle(cpu_id: usize) -> bool {
    with_cpu_scheduler(cpu_id, |sched| sched.effective_load() == 0).unwrap_or(false)
}

/// Select the best CPU for a waking task.
///
/// Inspired by Linux `select_task_rq_fair()` / `wake_affine_idle()`:
///   1. Prefer `last_cpu` if it has no queued work (cache locality + idle).
///   2. Prefer the waker's CPU if idle and affinity-compatible.
///   3. Fall through to the globally least-loaded CPU.
///   4. Last resort: `last_cpu` even if busy (keeps the task runnable).
pub fn select_target_cpu(task: &Task) -> Option<usize> {
    let current_cpu = slopos_arch::pcr::get_current_cpu();
    let affinity = task_cpu_affinity(task).unwrap_or(0);
    let last_cpu = task_last_cpu(task).map(|c| c as usize).unwrap_or(0);

    // 1. Prefer last_cpu when idle — cache-warm data is still there and
    //    no contention.  Mirrors Linux wake_affine_idle(): "If prev_cpu is
    //    idle and cache affine then avoid a migration."
    if is_schedulable_cpu(last_cpu, affinity) && cpu_is_idle(last_cpu) {
        return Some(last_cpu);
    }

    // 2. If the waker's CPU is idle and compatible, use it.  The waker is
    //    about to return to userspace or sleep, freeing the CPU shortly.
    //    Mirrors Linux WF_SYNC / wake_affine_idle() this_cpu path.
    if current_cpu != last_cpu
        && is_schedulable_cpu(current_cpu, affinity)
        && cpu_is_idle(current_cpu)
    {
        return Some(current_cpu);
    }

    // 3. Neither last_cpu nor waker CPU is idle — find globally least loaded.
    //    This spreads work across genuinely idle CPUs.
    if let Some(best_cpu) = find_least_loaded_cpu(affinity) {
        return Some(best_cpu);
    }

    // 4. Fallback: last_cpu even if busy — keeps the task runnable.
    if is_schedulable_cpu(last_cpu, affinity) {
        return Some(last_cpu);
    }

    // Boot-time fallback: allow queueing onto the current CPU before it is
    // marked online/enabled, so pre-init tasks can be staged before enter_scheduler().
    if is_local_enqueue_fallback_cpu(current_cpu, affinity) {
        return Some(current_cpu);
    }

    // Last resort, mirroring Linux `select_fallback_rq`: no *schedulable* CPU in
    // the mask right now, but a permitted CPU may be merely transiently
    // non-schedulable (an AP paused for a teardown, or mid-enable). Target it
    // anyway rather than dropping to `None` and stranding the wake — the remote
    // push parks it in that CPU's inbox and its next drain (idle-loop, tick, or
    // reschedule IPI) runs it. Prefer `last_cpu` (cache-warm), else the
    // lowest-index permitted online CPU. Only a mask with no online CPU at all
    // yields `None`.
    if slopos_arch::pcr::is_cpu_online(last_cpu) && affinity_allows_cpu(affinity, last_cpu) {
        return Some(last_cpu);
    }
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        if affinity_allows_cpu(affinity, cpu_id) && slopos_arch::pcr::is_cpu_online(cpu_id) {
            return Some(cpu_id);
        }
    }

    None
}

/// Select the best CPU for a **newly created** task (fork, spawn, exec).
///
/// Mirrors Linux's `WF_FORK` / `SD_BALANCE_FORK` slow path: bypasses
/// `last_cpu` entirely (cache is cold for a new address space) and finds
/// the globally idlest CPU.  A round-robin counter rotates the scan start
/// so sequential forks spread evenly when all CPUs have equal load.
pub fn select_target_cpu_for_new(task: &Task) -> Option<usize> {
    let current_cpu = slopos_arch::pcr::get_current_cpu();
    let affinity = task_cpu_affinity(task).unwrap_or(0);

    // Go straight to the global idlest-CPU search — no last_cpu preference.
    if let Some(best_cpu) = find_idlest_cpu(affinity) {
        return Some(best_cpu);
    }

    // Fallback: current CPU if schedulable.
    if is_schedulable_cpu(current_cpu, affinity) {
        return Some(current_cpu);
    }

    if is_local_enqueue_fallback_cpu(current_cpu, affinity) {
        return Some(current_cpu);
    }

    None
}

/// Find the CPU with the lowest effective load, using a round-robin starting
/// position to break ties.  This mirrors Linux's `for_each_cpu_wrap()` in
/// `sched_balance_find_dst_group_cpu()`.
///
/// The RR counter advances over the eligible set (not raw cpu_count) so
/// that tie-breaking is fair when some CPUs are ineligible.
fn find_idlest_cpu(affinity: u32) -> Option<usize> {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    if cpu_count == 0 {
        return None;
    }

    // Heap-allocate the eligible-CPU list: a stack [usize; MAX_CPUS]
    // is 2 KiB on its own and pushes this hot path over the
    // stack-sizes gate.
    let mut eligible = match slopos_ostd::KVec::<usize>::zeroed(slopos_arch::MAX_CPUS) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let mut n_eligible = 0usize;
    for cpu_id in 0..cpu_count {
        if is_schedulable_cpu(cpu_id, affinity) {
            eligible[n_eligible] = cpu_id;
            n_eligible += 1;
        }
    }
    if n_eligible == 0 {
        return None;
    }

    // Rotate start position so sequential calls spread across eligible CPUs.
    let start = FORK_RR_COUNTER.fetch_add(1, Ordering::Relaxed) % n_eligible;

    let mut best_cpu: Option<usize> = None;
    let mut min_load = u32::MAX;

    for i in 0..n_eligible {
        let cpu_id = eligible[(start + i) % n_eligible];

        if let Some(load) = with_cpu_scheduler(cpu_id, |sched| sched.effective_load()) {
            if load < min_load {
                min_load = load;
                best_cpu = Some(cpu_id);
            }
        }
    }

    best_cpu
}

#[inline]
fn cpu_matches_affinity(cpu_id: usize, affinity: u32) -> bool {
    affinity_allows_cpu(affinity, cpu_id)
}

#[inline]
pub(crate) fn affinity_mask_for_cpu(cpu_id: usize) -> u32 {
    if cpu_id >= u32::BITS as usize {
        0
    } else {
        1u32 << cpu_id
    }
}

#[inline]
pub(crate) fn affinity_allows_cpu(affinity: u32, cpu_id: usize) -> bool {
    if affinity == 0 {
        return true;
    }

    let mask = affinity_mask_for_cpu(cpu_id);
    mask != 0 && (affinity & mask) != 0
}

fn is_schedulable_cpu(cpu_id: usize, affinity: u32) -> bool {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    if cpu_id >= cpu_count {
        return false;
    }

    if !cpu_matches_affinity(cpu_id, affinity) {
        return false;
    }

    if !is_percpu_scheduler_initialized(cpu_id) {
        return false;
    }

    if !slopos_arch::pcr::is_cpu_online(cpu_id) {
        return false;
    }

    with_cpu_scheduler(cpu_id, |sched| sched.is_enabled()).unwrap_or(false)
}

fn is_local_enqueue_fallback_cpu(cpu_id: usize, affinity: u32) -> bool {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    if cpu_id >= cpu_count {
        return false;
    }

    if !cpu_matches_affinity(cpu_id, affinity) {
        return false;
    }

    is_percpu_scheduler_initialized(cpu_id)
}

fn find_least_loaded_cpu(affinity: u32) -> Option<usize> {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    let mut best_cpu: Option<usize> = None;
    let mut min_load = u32::MAX;

    for cpu_id in 0..cpu_count {
        if !is_schedulable_cpu(cpu_id, affinity) {
            continue;
        }

        // Use effective_load (queued + running) so that a CPU running a
        // real task is not considered equally idle to a truly idle CPU.
        if let Some(load) = with_cpu_scheduler(cpu_id, |sched| sched.effective_load()) {
            if load < min_load {
                min_load = load;
                best_cpu = Some(cpu_id);
            }
        }
    }

    best_cpu
}

/// Get the return context for an AP to use when no tasks are available.
/// This is stored in the per-CPU scheduler and initialized during AP startup.
pub fn get_ap_return_context(cpu_id: usize) -> *mut SwitchContext {
    cpu_scheduler(cpu_id)
        .map(|sched| sched.return_context.get())
        .unwrap_or(ptr::null_mut())
}

/// Whether `task` is the idle task of any CPU.
///
/// Pure identity: takes the compare-only [`TaskAddr`] rather than a pointer,
/// so the answer cannot be used as a licence to read the task.
///
/// PCR.idle_task is the source of truth post-consolidation; the
/// scheduler-copy field is kept in lockstep by `install_idle_task`
/// until its deletion in a follow-up commit.
pub fn is_idle_task(task: TaskAddr) -> bool {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    (0..cpu_count).any(|cpu_id| TaskAddr::idle_of(cpu_id) == Some(task))
}

// =============================================================================
// AP Pause Mechanism for Test Reinitialization
// =============================================================================

/// Global flag to pause all AP scheduler loops during test reinitialization.
/// When set, APs will spin-wait instead of processing tasks.
static AP_PAUSED: AtomicBool = AtomicBool::new(false);

pub fn pause_all_aps() -> bool {
    let was_paused = AP_PAUSED.swap(true, Ordering::SeqCst);
    if !was_paused {
        core::sync::atomic::fence(Ordering::SeqCst);
        let cpu_count = slopos_arch::pcr::get_cpu_count();
        let max_wait_iterations = 100_000;
        for iteration in 0..max_wait_iterations {
            let mut all_idle = true;
            for cpu_id in 1..cpu_count {
                if let Some(executing) =
                    with_cpu_scheduler(cpu_id, |sched| sched.is_executing_task())
                {
                    if executing {
                        all_idle = false;
                        break;
                    }
                }
            }
            if all_idle {
                break;
            }
            if iteration < 1000 {
                core::hint::spin_loop();
            }
        }
    }
    was_paused
}

pub fn resume_all_aps() {
    core::sync::atomic::fence(Ordering::SeqCst);
    AP_PAUSED.store(false, Ordering::SeqCst);

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 1..cpu_count {
        // Wake an AP that has ready work OR a cross-core wake parked in its
        // inbox. Gating only on the ready count would leave an inbox-parked
        // wake (pushed while this AP was paused during a task teardown) waiting
        // for the next timer tick instead of resuming promptly on the IPI.
        if let Some((ready, inbox)) = with_cpu_scheduler(cpu_id, |sched| {
            (sched.total_ready_count(), sched.inbox_count())
        }) {
            if ready > 0 || inbox > 0 {
                if let Some(apic_id) = slopos_arch::pcr::apic_id_from_cpu_index(cpu_id) {
                    slopos_arch::pcr::send_ipi_to_cpu(
                        apic_id,
                        slopos_arch::arch::idt::RESCHEDULE_IPI_VECTOR,
                    );
                }
            }
        }
    }
}

pub fn resume_all_aps_if_not_nested(was_already_paused: bool) {
    if !was_already_paused {
        resume_all_aps();
    }
}

/// Check if APs should be paused.
#[inline]
pub fn are_aps_paused() -> bool {
    AP_PAUSED.load(Ordering::Acquire)
}

#[inline]
pub fn should_pause_scheduler_loop(cpu_id: usize) -> bool {
    cpu_id != 0 && are_aps_paused()
}

/// Clear all ready queues for a specific CPU. Used during test reinitialization.
pub fn clear_cpu_queues(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    let Some(sched) = cpu_scheduler(cpu_id) else {
        return;
    };
    let _guard = sched.queue_lock.lock();
    for queue in &sched.ready_queues {
        queue.clear_with_ref_release();
    }
    drop(_guard);
    sched.clear_remote_inbox_with_ref_release();
}

/// Clear all per-CPU ready queues across all CPUs. Used during scheduler shutdown.
pub fn clear_all_cpu_queues() {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        clear_cpu_queues(cpu_id);
    }
}
