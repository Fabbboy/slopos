//! Per-CPU scheduler: each CPU owns a [`PriorityRunQueue`] with local run
//! queues.
//!
//! The run queue is interior-mutable throughout so every public API takes
//! `&self`. `ready_queues` operations are serialised by `queue_lock` because
//! the intrusive list is not lock-free across operations.

use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use slopos_ostd::lock_class;

/// Round-robin start position for fork/spawn CPU placement, so sequential
/// forks spread across CPUs even when all carry the same load.
static FORK_RR_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "test-hooks")]
pub fn fork_rr_counter_value() -> usize {
    FORK_RR_COUNTER.load(Ordering::Relaxed)
}

#[cfg(feature = "test-hooks")]
pub fn fork_rr_counter_set(value: usize) {
    FORK_RR_COUNTER.store(value, Ordering::Relaxed);
}

use super::task::{TaskRef, task_put};
use super::task_struct::Task;
use crate::fair;
use slopos_abi::task::TaskStatus;
use slopos_arch::MAX_CPUS;
use slopos_ostd::sync::intrusive::{IntrusiveLinkedList, LinkError};
use slopos_ostd::sync::{InitFlag, KernelSync, LOCK_LEVEL_SCHEDULER, SpinLock};
use slopos_ostd::task::{SchedPlacement, TaskAddr, task_placement_retain, with_parked_node};
use slopos_ostd::{klog_debug, klog_info};

/// One slot per [`TaskPriority`] variant; each variant's repr value is its
/// index into [`PriorityRunQueue::ready_queues`].
const NUM_PRIORITY_LEVELS: usize = 5;

/// Role tag for the per-CPU `ReadyQueue` intrusive list. Defined in
/// OSTD so the kernel `TaskInner<K, U>` can `impl LinkProvider` against
/// it without OSTD reaching into `core/`.
pub use slopos_ostd::task::link_roles::ReadyQueueRole;

use slopos_ostd::sync::kernel_io_task::{KernelIoTaskIds, MAX_KERNEL_IO_STOPS};
#[cfg(feature = "test-hooks")]
use slopos_ostd::sync::kernel_io_task::{kernel_io_hold_armed, kernel_io_hold_covers};

/// Per-priority FIFO of ready tasks.
struct ReadyQueue {
    list: KernelSync<IntrusiveLinkedList<Task, ReadyQueueRole>>,
}

impl ReadyQueue {
    const fn new() -> Self {
        Self {
            list: KernelSync::new(IntrusiveLinkedList::new()),
        }
    }

    /// Registered kernel-I/O threads are re-linked: the reference dropped here
    /// is the *owning* one, so the caller could not put it back.
    fn clear_with_ref_release(&self, kernel_io: &KernelIoTaskIds) {
        let mut preserved: [Option<NonNull<Task>>; MAX_KERNEL_IO_STOPS] =
            [None; MAX_KERNEL_IO_STOPS];
        let mut kept = 0usize;
        while let Some(node) = self.list.pop() {
            let task = TaskRef::from_placement(node);
            if crate::task::kernel_io_hold_claim(&task, SchedPlacement::ReadyQueue) {
                task_put(task);
                continue;
            }
            if kept < preserved.len() && kernel_io.contains(task.task_id) {
                // Re-linking here would push onto the list this loop drains.
                preserved[kept] = Some(node);
                kept += 1;
                core::mem::forget(task);
                continue;
            }
            let _ = task
                .sched_placement_compare_exchange(SchedPlacement::ReadyQueue, SchedPlacement::None);
            task_put(task);
        }
        for node in preserved[..kept].iter().flatten() {
            if self.list.push(*node).is_err() {
                let task = TaskRef::from_placement(*node);
                let _ = task.sched_placement_compare_exchange(
                    SchedPlacement::ReadyQueue,
                    SchedPlacement::None,
                );
                task_put(task);
            }
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
    /// `SchedPlacement::ReadyQueue`. Membership mints an owning reference,
    /// which is sound because `&TaskRef` proves the caller holds a live one.
    ///
    /// Returns:
    /// - `0` when the task was newly linked and its owning reference parked;
    /// - `1` when it was already linked in a ready queue;
    /// - `-1` on an unexpected list error.
    fn link_preclaimed_with_status(&self, task: &TaskRef) -> i32 {
        let node = task.node();
        match self.list.push(node) {
            Ok(()) => {
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
    /// dispatcher needs the task pinned across the whole window between
    /// dequeue and the switch.
    fn dequeue(&self) -> Option<TaskRef> {
        let node = self.list.pop()?;
        let task = TaskRef::from_placement(node);
        let _ = task
            .sched_placement_compare_exchange(SchedPlacement::ReadyQueue, SchedPlacement::OnCpu);
        Some(task)
    }

    /// Unlink a task and release the owning reference membership held.
    fn remove(&self, task: &Task) -> i32 {
        // Reclaim from the list's own link pointer, not from `task`: the
        // reclaim walks backwards into the `KArc` header, which a pointer
        // derived from a `&Task` has no provenance over.
        let Ok(node) = self.list.remove(NonNull::from(task)) else {
            return -1;
        };
        let _ =
            task.sched_placement_compare_exchange(SchedPlacement::ReadyQueue, SchedPlacement::None);
        task_put(TaskRef::from_placement(node));
        0
    }

    #[cfg(feature = "test-hooks")]
    fn hold_kernel_io(&self) -> usize {
        let mut found: [Option<NonNull<Task>>; MAX_KERNEL_IO_STOPS] = [None; MAX_KERNEL_IO_STOPS];
        let mut seen = 0usize;
        for node in self.list.iter() {
            if seen == found.len() {
                break;
            }
            if with_parked_node(node, |task| kernel_io_hold_covers(task.task_id)) {
                found[seen] = Some(node);
                seen += 1;
            }
        }

        let mut held = 0usize;
        for node in found[..seen].iter().flatten() {
            let claimed = with_parked_node(*node, |task| {
                task.sched_placement_compare_exchange(
                    SchedPlacement::ReadyQueue,
                    SchedPlacement::Held,
                )
            });
            if !claimed {
                continue;
            }
            if self.list.remove(*node).is_err() {
                with_parked_node(*node, |task| {
                    let _ = task.sched_placement_compare_exchange(
                        SchedPlacement::Held,
                        SchedPlacement::ReadyQueue,
                    );
                });
                continue;
            }
            task_put(TaskRef::from_placement(*node));
            held += 1;
        }
        held
    }

    /// Detach the tail task for migration, handing the thief the owning
    /// reference this queue held.
    fn steal_from_tail(&self) -> Option<TaskRef> {
        if self.list.len() <= 1 {
            return None;
        }
        let last = self.list.iter().last()?;
        // Borrowed, not reclaimed: both CASes below can fail, and taking the
        // reference would release a membership the queue keeps.
        let claimed = with_parked_node(last, |task| {
            task.sched_placement_compare_exchange(
                SchedPlacement::ReadyQueue,
                SchedPlacement::Migrating,
            )
        });
        if !claimed {
            return None;
        }
        if self.list.remove(last).is_err() {
            with_parked_node(last, |task| {
                let _ = task.sched_placement_compare_exchange(
                    SchedPlacement::Migrating,
                    SchedPlacement::ReadyQueue,
                );
            });
            return None;
        }
        Some(TaskRef::from_placement(last))
    }
}

const EMPTY_QUEUE: ReadyQueue = ReadyQueue::new();

#[repr(C, align(64))]
pub struct PriorityRunQueue {
    /// Owning CPU id, written once during `init`.
    cpu_id_atom: AtomicUsize,
    ready_queues: [ReadyQueue; NUM_PRIORITY_LEVELS],
    /// Anti-starvation backstop. Guarded by `queue_lock`, which also
    /// serialises the `ready_queues` it describes.
    aging: KernelSync<fair::AgingState>,
    queue_lock: SpinLock<()>,
    pub enabled: AtomicBool,
    /// Default time slice in ticks, written once during `init`.
    time_slice_atom: AtomicU32,
    pub total_switches: AtomicU64,
    pub total_preemptions: AtomicU64,
    pub total_ticks: AtomicU64,
    pub idle_time: AtomicU64,
    pub total_yields: AtomicU64,
    pub schedule_calls: AtomicU32,
    initialized: AtomicBool,
    executing_task: AtomicBool,
    remote_inbox_head: AtomicPtr<Task>,
    inbox_count: AtomicU32,
}

impl PriorityRunQueue {
    pub const fn new() -> Self {
        Self {
            cpu_id_atom: AtomicUsize::new(0),
            ready_queues: [EMPTY_QUEUE; NUM_PRIORITY_LEVELS],
            aging: KernelSync::new(fair::AgingState::new()),
            queue_lock: SpinLock::new(
                (),
                lock_class!("PriorityRunQueue.queue_lock", LOCK_LEVEL_SCHEDULER),
            ),
            enabled: AtomicBool::new(false),
            time_slice_atom: AtomicU32::new(10),
            total_switches: AtomicU64::new(0),
            total_preemptions: AtomicU64::new(0),
            total_ticks: AtomicU64::new(0),
            idle_time: AtomicU64::new(0),
            total_yields: AtomicU64::new(0),
            schedule_calls: AtomicU32::new(0),
            initialized: AtomicBool::new(false),
            executing_task: AtomicBool::new(false),
            remote_inbox_head: AtomicPtr::new(ptr::null_mut()),
            inbox_count: AtomicU32::new(0),
        }
    }

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

    /// Initialise this CPU's scheduler. Must run on the owning CPU before any
    /// task is enqueued onto it; idempotent, so test-fixture re-init shares
    /// this path.
    pub fn init(&self, cpu_id: usize) {
        let kernel_io = slopos_ostd::sync::kernel_io_task::kernel_io_task_ids();
        self.cpu_id_atom.store(cpu_id, Ordering::Relaxed);
        self.time_slice_atom.store(10, Ordering::Relaxed);
        for queue in &self.ready_queues {
            queue.clear_with_ref_release(&kernel_io);
        }
        self.enabled.store(false, Ordering::Relaxed);
        self.total_switches.store(0, Ordering::Relaxed);
        self.total_preemptions.store(0, Ordering::Relaxed);
        self.total_ticks.store(0, Ordering::Relaxed);
        self.idle_time.store(0, Ordering::Relaxed);
        self.total_yields.store(0, Ordering::Relaxed);
        self.schedule_calls.store(0, Ordering::Relaxed);
        self.aging.reset();
        self.initialized.store(true, Ordering::Release);
        self.clear_remote_inbox_with_ref_release(&kernel_io);
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

    /// Park a migrating task on this CPU; the caller keeps and separately
    /// releases the handle it carried across the `Migrating` window.
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
        // `-1`, not `1`: `1` means "some queue already owns it", which would
        // make the publish fallback believe this never-published task landed
        // somewhere.
        if current == SchedPlacement::Nascent {
            return -1;
        }
        if current == SchedPlacement::Held {
            return 1;
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
        // Claim, not refuse: `from` is a placement the caller owns, unseen by a `Held` refusal.
        if crate::task::kernel_io_hold_claim(body, from) {
            return 1;
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

        let mut non_empty = [false; fair::NUM_TIERS];
        for (tier, queue) in self.ready_queues.iter().enumerate() {
            if tier < fair::NUM_TIERS {
                non_empty[tier] = !queue.is_empty();
            }
        }

        // A tier that has been passed over its whole budget is served before
        // the strict scan runs, which is the only thing that bounds its wait.
        if let Some(tier) = self.aging.tier_owed(&non_empty)
            && let Some(task) = self.ready_queues[tier].dequeue()
        {
            self.aging.note_dispatch(tier, &non_empty);
            return Some(task);
        }

        for (tier, queue) in self.ready_queues.iter().enumerate() {
            if let Some(task) = queue.dequeue() {
                self.aging.note_dispatch(tier, &non_empty);
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

    /// Pending cross-core wakes parked in this CPU's remote inbox. A non-null
    /// head counts as at least one entry: `push_remote_wake` links the head
    /// before bumping `inbox_count`, so a bare count can momentarily
    /// undercount.
    pub fn inbox_count(&self) -> u32 {
        let inbox = self.inbox_count.load(Ordering::Relaxed);
        if inbox == 0 && !self.remote_inbox_head.load(Ordering::Acquire).is_null() {
            1
        } else {
            inbox
        }
    }

    /// Effective load on this CPU: queued tasks plus one if a non-idle task is
    /// currently running. Lock-free and approximate.
    pub fn effective_load(&self) -> u32 {
        let queued: u32 = self.ready_queues.iter().map(|q| q.len()).sum();
        let inbox = self.inbox_count.load(Ordering::Relaxed);
        let inbox = if inbox == 0 && !self.remote_inbox_head.load(Ordering::Acquire).is_null() {
            1
        } else {
            inbox
        };
        let cpu_id = self.cpu_id();
        // The bootstrap stub is eight bytes with no task identity to compare,
        // so that check reads the slot raw and recognises it by address range.
        let raw_current = slopos_arch::pcr::get_current_task_for(cpu_id);
        let current = TaskAddr::current_of(cpu_id);
        let running_real = current.is_some()
            && !slopos_ostd::task::bootstrap::is_bootstrap_task_ptr(raw_current.cast_const())
            && current != TaskAddr::idle_of(cpu_id);
        let load = queued.saturating_add(inbox);
        if running_real {
            load.saturating_add(1)
        } else {
            load
        }
    }

    /// Clear the remote inbox and its count. Test fixtures only.
    #[cfg(feature = "test-hooks")]
    pub fn force_clear_inbox_count(&self) {
        // Off-lock: the stop registry ranks with the task registry, not with a run queue.
        let kernel_io = slopos_ostd::sync::kernel_io_task::kernel_io_task_ids();
        self.clear_remote_inbox_with_ref_release(&kernel_io);
        self.inbox_count.store(0, Ordering::Relaxed);
    }

    #[cfg(feature = "test-hooks")]
    fn hold_kernel_io(&self) -> usize {
        let _guard = self.queue_lock.lock();
        self.ready_queues
            .iter()
            .map(|queue| queue.hold_kernel_io())
            .sum()
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

    /// Push a task to this CPU's remote wake inbox. Lock-free MPSC push,
    /// callable from any CPU.
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

        // The single gate against ready-queue + remote-inbox double placement:
        // a task already scheduler-owned in any placement makes a duplicate
        // wake a no-op.
        let current = body.sched_placement();
        if current == SchedPlacement::ReadyQueue
            || current == SchedPlacement::RemoteWake
            || current == SchedPlacement::OnCpu
            || current == SchedPlacement::Migrating
            || current == SchedPlacement::Held
            || (current == SchedPlacement::Waking && from != SchedPlacement::Waking)
        {
            return 1;
        }
        if current != from {
            return -1;
        }
        if crate::task::kernel_io_hold_claim(body, from) {
            return 1;
        }
        if !body.sched_placement_compare_exchange(from, SchedPlacement::RemoteWake) {
            let after = body.sched_placement();
            if after != SchedPlacement::None {
                return 1;
            }
            return -1;
        }

        // A tail node has a null successor while still queued, so membership is
        // tracked by the link's `linked` bit rather than inferred from it.
        if !body.inbox_link().try_mark_linked() {
            let _ = body.sched_placement_compare_exchange(SchedPlacement::RemoteWake, from);
            return -1;
        }

        // Park the inbox's reference before publishing the node: a drain that
        // immediately swaps the head must not drop the last reference before
        // the producer has finished linking it.
        body.set_last_cpu(self.cpu_id() as u8);
        task_placement_retain(node);

        loop {
            let old_head = self.remote_inbox_head.load(Ordering::Acquire);

            // The inbox reference parked above is what keeps this borrow live.
            body.inbox_link().store_relaxed(old_head);

            match self.remote_inbox_head.compare_exchange_weak(
                old_head,
                node.as_ptr(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.inbox_count.fetch_add(1, Ordering::Relaxed);
                    return 0;
                }
                Err(_) => {
                    core::hint::spin_loop();
                }
            }
        }
    }

    /// Drain all tasks from remote inbox into local ready queues.
    /// MUST only be called by the owning CPU.
    ///
    /// Each node's parked reference is reclaimed first — refcount-neutral, the
    /// inverse of the producer's park — so every status read, placement CAS and
    /// enqueue below is made through a handle this CPU owns.
    pub fn drain_remote_inbox(&self) {
        let head = self
            .remote_inbox_head
            .swap(ptr::null_mut(), Ordering::AcqRel);

        if head.is_null() {
            return;
        }

        // Reversing turns the producers' LIFO push order into FIFO drain order.
        let (mut cursor, count) = slopos_ostd::task::reverse_detached_chain::<
            Task,
            slopos_ostd::task::RemoteWakeRole,
        >(head);

        while let Some(node) = NonNull::new(cursor) {
            let task = TaskRef::from_placement(node);
            let body: &Task = &task;
            let next = body.inbox_link().load();
            body.inbox_link().store(ptr::null_mut());

            if crate::task::kernel_io_hold_claim(body, SchedPlacement::RemoteWake) {
                body.inbox_link().mark_unlinked();
                task_put(task);
                cursor = next;
                continue;
            }

            if body.status() == (TaskStatus::Ready)
                && body.sched_placement_compare_exchange(
                    SchedPlacement::RemoteWake,
                    SchedPlacement::ReadyQueue,
                )
            {
                // The task is scheduler-owned as ReadyQueue for this handoff
                // even before `ready_link` is linked, so a duplicate wake
                // cannot publish a second entry.
                body.inbox_link().mark_unlinked();
                if self.enqueue_local_preclaimed(&task) < 0 {
                    let _ = body.sched_placement_compare_exchange(
                        SchedPlacement::ReadyQueue,
                        SchedPlacement::None,
                    );
                }
            } else {
                // A wake that raced while producers saw `RemoteWake` was a
                // no-op, so this CPU re-checks after dropping the claim; a
                // producer that enqueued after the release leaves a non-None
                // placement and `enqueue_local` no-ops.
                body.inbox_link().mark_unlinked();
                let _ = body.sched_placement_compare_exchange(
                    SchedPlacement::RemoteWake,
                    SchedPlacement::None,
                );
                if body.status() == (TaskStatus::Ready) {
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

    /// Discard the whole inbox, releasing each parked reference.
    fn clear_remote_inbox_with_ref_release(&self, kernel_io: &KernelIoTaskIds) {
        let mut cursor = self
            .remote_inbox_head
            .swap(ptr::null_mut(), Ordering::AcqRel);
        let mut drained = 0u32;
        while let Some(node) = NonNull::new(cursor) {
            let task = TaskRef::from_placement(node);
            let body: &Task = &task;
            let next = body.inbox_link().load();
            body.inbox_link().store(ptr::null_mut());
            body.inbox_link().mark_unlinked();
            cursor = next;
            drained = drained.saturating_add(1);

            if crate::task::kernel_io_hold_claim(body, SchedPlacement::RemoteWake) {
                task_put(task);
                continue;
            }
            if kernel_io.contains(body.task_id)
                && body.status() == (TaskStatus::Ready)
                && body.sched_placement_compare_exchange(
                    SchedPlacement::RemoteWake,
                    SchedPlacement::ReadyQueue,
                )
            {
                if self.enqueue_local_preclaimed(&task) < 0 {
                    let _ = body.sched_placement_compare_exchange(
                        SchedPlacement::ReadyQueue,
                        SchedPlacement::None,
                    );
                }
                task_put(task);
                continue;
            }
            let _ = body
                .sched_placement_compare_exchange(SchedPlacement::RemoteWake, SchedPlacement::None);
            task_put(task);
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

    #[inline]
    pub fn has_pending_inbox(&self) -> bool {
        !self.remote_inbox_head.load(Ordering::Acquire).is_null()
    }
}

use slopos_ostd::sync::CacheAligned;
use slopos_ostd::sync::cpu_local::CpuLocal;

/// The global preemptive priority scheduler: one [`PriorityRunQueue`] per CPU
/// through [`CpuLocal`], which pins and cache-line-aligns each slot. The
/// preemptive surface lives as free functions in [`crate::scheduler`].
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

    /// Borrow the per-CPU [`PriorityRunQueue`] for `cpu_id`. Cross-CPU reads
    /// are valid because every interior field is atomic or `SpinLock`-guarded.
    #[inline]
    pub fn runqueue_for(&'static self, cpu_id: usize) -> Option<&'static PriorityRunQueue> {
        self.runqueues.snapshot_for_cpu(cpu_id)
    }
}

pub static PRIORITY_SCHEDULER: PriorityScheduler = PriorityScheduler::new();

#[inline]
fn cpu_scheduler(cpu_id: usize) -> Option<&'static PriorityRunQueue> {
    PRIORITY_SCHEDULER.runqueue_for(cpu_id)
}

/// `init_all_percpu_schedulers` init-once gate. `pub(crate)` for the
/// `test_hermetic::SchedulersInitFlag` snapshot/restore.
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

/// Release `cpu_id`'s dispatch flag on its behalf, from that CPU's own
/// non-returning abort path.
///
/// Deliberately leaves the task `cpu_id` was running alone: that drop must run
/// on the task's own stack, interrupts enabled and no lock held (I3), none of
/// which this context can provide.
pub fn abandon_dispatch_for_dying_cpu(cpu_id: usize) {
    let _ = with_cpu_scheduler(cpu_id, |sched| sched.set_executing_task(false));
}

pub fn enqueue_task_on_cpu(cpu_id: usize, task: &TaskRef) -> i32 {
    if cpu_id >= MAX_CPUS {
        return -1;
    }

    let body: &Task = task;
    if body.status() != (TaskStatus::Ready) {
        return -1;
    }

    with_cpu_scheduler(cpu_id, |sched| match body.sched_placement() {
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

/// Genuinely idle: no queued tasks and no real (non-idle) task running.
fn cpu_is_idle(cpu_id: usize) -> bool {
    with_cpu_scheduler(cpu_id, |sched| sched.effective_load() == 0).unwrap_or(false)
}

/// Select the best CPU for a waking task.
pub fn select_target_cpu(task: &Task) -> Option<usize> {
    let current_cpu = slopos_arch::pcr::get_current_cpu();
    let affinity = task.cpu_affinity();
    let last_cpu = task.last_cpu() as usize;

    // Prefer `last_cpu` when idle: its cache-warm data is still there.
    if is_schedulable_cpu(last_cpu, affinity) && cpu_is_idle(last_cpu) {
        return Some(last_cpu);
    }

    // The waker is about to return to userspace or sleep, freeing its CPU.
    if current_cpu != last_cpu
        && is_schedulable_cpu(current_cpu, affinity)
        && cpu_is_idle(current_cpu)
    {
        return Some(current_cpu);
    }

    if let Some(best_cpu) = find_least_loaded_cpu(affinity) {
        return Some(best_cpu);
    }

    if is_schedulable_cpu(last_cpu, affinity) {
        return Some(last_cpu);
    }

    // Boot-time: stage pre-init tasks on the current CPU before it is marked
    // online/enabled.
    if is_local_enqueue_fallback_cpu(current_cpu, affinity) {
        return Some(current_cpu);
    }

    // Last resort: a permitted CPU may be merely transiently non-schedulable
    // (an AP paused for a teardown, or mid-enable). Target it anyway rather
    // than stranding the wake — the remote push parks it in that CPU's inbox
    // and its next drain runs it. Only a mask with no online CPU yields `None`.
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
/// Bypasses `last_cpu` entirely — cache is cold for a new address space — and
/// goes straight to the globally idlest CPU.
pub fn select_target_cpu_for_new(task: &Task) -> Option<usize> {
    let current_cpu = slopos_arch::pcr::get_current_cpu();
    let affinity = task.cpu_affinity();

    if let Some(best_cpu) = find_idlest_cpu(affinity) {
        return Some(best_cpu);
    }

    if is_schedulable_cpu(current_cpu, affinity) {
        return Some(current_cpu);
    }

    if is_local_enqueue_fallback_cpu(current_cpu, affinity) {
        return Some(current_cpu);
    }

    None
}

/// Find the CPU with the lowest effective load, using a round-robin starting
/// position to break ties. The counter advances over the eligible set, not raw
/// `cpu_count`, so tie-breaking stays fair when some CPUs are ineligible.
fn find_idlest_cpu(affinity: u32) -> Option<usize> {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    if cpu_count == 0 {
        return None;
    }

    // Heap, not stack: a `[usize; MAX_CPUS]` is 2 KiB on its own and pushes
    // this frame over the stack-sizes gate.
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

        if let Some(load) = with_cpu_scheduler(cpu_id, |sched| sched.effective_load()) {
            if load < min_load {
                min_load = load;
                best_cpu = Some(cpu_id);
            }
        }
    }

    best_cpu
}

/// Whether `task` is the idle task of any CPU.
pub fn is_idle_task(task: TaskAddr) -> bool {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    (0..cpu_count).any(|cpu_id| TaskAddr::idle_of(cpu_id) == Some(task))
}

// The AP pause parks every AP at its scheduler-loop poll point so the BSP can
// mutate kernel-wide scheduler state unraced — the shutdown task sweep and the
// hermetic test scope's snapshot/reset window.

/// Nesting depth of the AP pause; non-zero parks every AP at its poll point. A
/// count rather than a flag: under a flag, the first of two overlapping pauses
/// to release would lift the second holder's pause out from under it.
static AP_PAUSE_DEPTH: AtomicU32 = AtomicU32::new(0);

/// Spin budget for the pause wait. Each iteration is one scan of the online APs
/// plus a `spin_loop` hint, so this bounds work, not wall-clock time.
const AP_PAUSE_SPIN_BUDGET: u32 = 100_000;

/// Spin iterations between reschedule-IPI re-sends: an IPI that is lost or
/// coalesced against a pending one would leave the wait spinning for an AP that
/// was never provoked.
const AP_PAUSE_NUDGE_INTERVAL: u32 = 16_384;

/// Proof that one AP pause is held. `Drop` performs the same release as
/// [`resume_all_aps_if_not_nested`], so a panic between acquire and release
/// cannot park every AP permanently.
#[must_use = "the pause is held until the token is released"]
pub struct ApPauseToken {
    _private: (),
}

impl Drop for ApPauseToken {
    fn drop(&mut self) {
        release_ap_pause_depth();
    }
}

/// Why an AP pause could not be established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApPauseError {
    /// `cpu_id` was still executing a task after the whole spin budget. The
    /// depth increment is rolled back first, so there is nothing to release.
    Timeout { cpu_id: usize },
}

/// Park every AP's scheduler loop and wait until none is executing a task.
///
/// Nests: an inner call joins the pause already in effect and returns without
/// waiting, and the APs stay parked until the last token is released.
///
/// Returns `Err` if an AP is still executing after the spin budget. A caller
/// whose correctness rests on the APs being quiescent must treat that as a hard
/// failure rather than proceed against APs still free to race it.
pub fn pause_all_aps() -> Result<ApPauseToken, ApPauseError> {
    let outermost = AP_PAUSE_DEPTH.fetch_add(1, Ordering::SeqCst) == 0;
    if !outermost {
        return Ok(ApPauseToken { _private: () });
    }

    // The depth increment must be visible to an AP before this CPU reads that
    // AP's executing flag, or a CPU that dispatched just ahead of the increment
    // reads back as parked and the wait ends early.
    core::sync::atomic::fence(Ordering::SeqCst);

    match wait_for_aps_to_park(slopos_arch::pcr::get_cpu_count()) {
        Ok(()) => {
            let skipped = SKIPPED_OFFLINE_APS.load(Ordering::Relaxed);
            if skipped != 0 && SKIPPED_OFFLINE_REPORTED.enter() {
                klog_info!(
                    "SCHED: AP pause stepped over {} offline AP(s) still flagged as executing",
                    skipped
                );
            }
            Ok(ApPauseToken { _private: () })
        }
        Err(err) => {
            release_ap_pause_depth();
            Err(err)
        }
    }
}

static SKIPPED_OFFLINE_APS: AtomicU32 = AtomicU32::new(0);

#[cfg(feature = "test-hooks")]
pub fn skipped_offline_ap_count() -> u32 {
    SKIPPED_OFFLINE_APS.load(Ordering::Relaxed)
}
static SKIPPED_OFFLINE_REPORTED: slopos_ostd::sync::StateFlag = slopos_ostd::sync::StateFlag::new();

fn executing_ap(cpu_count: usize) -> Option<usize> {
    (1..cpu_count).find(|&cpu_id| {
        let executing = with_cpu_scheduler(cpu_id, |sched| sched.is_executing_task()) == Some(true);
        // Only that AP can clear its own flag, so waiting on an offline one is a
        // wait that cannot end.
        if executing && !slopos_arch::pcr::is_cpu_online(cpu_id) {
            SKIPPED_OFFLINE_APS.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        executing
    })
}

fn wait_for_aps_to_park(cpu_count: usize) -> Result<(), ApPauseError> {
    if executing_ap(cpu_count).is_none() {
        return Ok(());
    }

    // An AP holds `executing_task` for as long as its task runs, so waiting
    // unprovoked is a wait on that task yielding; the reschedule IPI turns it
    // into a wait on interrupt latency.
    nudge_aps_to_poll_point(cpu_count);

    for iteration in 0..AP_PAUSE_SPIN_BUDGET {
        if executing_ap(cpu_count).is_none() {
            return Ok(());
        }
        if iteration != 0 && iteration % AP_PAUSE_NUDGE_INTERVAL == 0 {
            nudge_aps_to_poll_point(cpu_count);
        }
        core::hint::spin_loop();
    }

    match executing_ap(cpu_count) {
        Some(cpu_id) => Err(ApPauseError::Timeout { cpu_id }),
        None => Ok(()),
    }
}

fn nudge_aps_to_poll_point(cpu_count: usize) {
    for cpu_id in 1..cpu_count {
        crate::lifecycle::send_reschedule_ipi(cpu_id);
    }
}

/// Drop one level of pause depth, waking the APs when the last one goes.
fn release_ap_pause_depth() {
    if AP_PAUSE_DEPTH.fetch_sub(1, Ordering::SeqCst) != 1 {
        return;
    }

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 1..cpu_count {
        // Gating only on the ready count would leave an inbox-parked wake
        // waiting for the next timer tick instead of resuming on the IPI.
        if let Some((ready, inbox)) = with_cpu_scheduler(cpu_id, |sched| {
            (sched.total_ready_count(), sched.inbox_count())
        }) {
            if ready > 0 || inbox > 0 {
                crate::lifecycle::send_reschedule_ipi(cpu_id);
            }
        }
    }
}

/// Release the pause `token` stands for. The APs resume only once the last
/// outstanding token is released.
pub fn resume_all_aps_if_not_nested(token: ApPauseToken) {
    drop(token);
}

#[inline]
pub fn are_aps_paused() -> bool {
    AP_PAUSE_DEPTH.load(Ordering::Acquire) != 0
}

#[inline]
pub fn ap_pause_depth() -> u32 {
    AP_PAUSE_DEPTH.load(Ordering::Acquire)
}

#[inline]
pub fn should_pause_scheduler_loop(cpu_id: usize) -> bool {
    cpu_id != 0 && are_aps_paused()
}

/// Clear one CPU's ready queues and its remote inbox.
pub fn clear_cpu_queues(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    let Some(sched) = cpu_scheduler(cpu_id) else {
        return;
    };
    // Before the queue lock: the stop registry is the same lock level.
    let kernel_io = slopos_ostd::sync::kernel_io_task::kernel_io_task_ids();
    // Drained, not discarded: a pending wake for a preserved thread must land
    // in the ready queue the retention below understands.
    if !kernel_io.is_empty() {
        sched.drain_remote_inbox();
    }
    let _guard = sched.queue_lock.lock();
    for queue in &sched.ready_queues {
        queue.clear_with_ref_release(&kernel_io);
    }
    drop(_guard);
    sched.clear_remote_inbox_with_ref_release(&kernel_io);
}

pub fn clear_all_cpu_queues() {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        clear_cpu_queues(cpu_id);
    }
}

/// The token is a precondition: `ReadyQueue::dequeue` ignores its own placement CAS,
/// so an AP still dispatching could run a task this sweep had already claimed.
#[cfg(feature = "test-hooks")]
pub fn hold_kernel_io_off_all_runqueues(_paused: &ApPauseToken) -> usize {
    if !kernel_io_hold_armed() {
        return 0;
    }
    let mut held = 0usize;
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        let Some(sched) = cpu_scheduler(cpu_id) else {
            continue;
        };
        // A drain, not a discard: a discarded wake leaves the task `Ready` with no placement.
        sched.drain_remote_inbox();
        held += sched.hold_kernel_io();
    }
    held
}
