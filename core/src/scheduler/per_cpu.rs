//! Per-CPU Scheduler for SMP Support
//!
//! Each CPU has its own scheduler instance with local run queues.
//! This minimizes lock contention and improves cache locality.
//!
//! # Safety Model
//!
//! `PerCpuScheduler` uses interior mutability throughout so that all public
//! APIs take `&self` (shared reference). This eliminates the UB that arose
//! from handing out `&mut` to a `static` array element from multiple CPUs.
//!
//! - Atomic fields: direct load/store (lock-free).
//! - `ready_queues`: wrapped in `UnsafeCell`, guarded by `queue_lock`.
//! - `return_context`: wrapped in `UnsafeCell`, only written during
//!   single-threaded init and read by the owning CPU.

use core::cell::{SyncUnsafeCell, UnsafeCell};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Round-robin counter for fork/spawn CPU placement.  Rotates the starting
/// position in `find_idlest_cpu()` so sequential forks spread across CPUs
/// even when all are idle (all have the same load).  Mirrors Linux's
/// `for_each_cpu_wrap()` pattern in `sched_balance_find_dst_group_cpu()`.
static FORK_RR_COUNTER: AtomicUsize = AtomicUsize::new(0);

use super::task_struct::{Task, TaskContext};
use slopos_abi::task::TaskStatus;
use slopos_arch::MAX_CPUS;
use slopos_sync::{InitFlag, IrqMutex};
use slopos_utils::{klog_debug, klog_info};

const NUM_PRIORITY_LEVELS: usize = 4;

#[derive(Default)]
struct ReadyQueue {
    head: *mut Task,
    tail: *mut Task,
    count: AtomicU32,
}

unsafe impl Send for ReadyQueue {}
unsafe impl Sync for ReadyQueue {}

impl ReadyQueue {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            count: AtomicU32::new(0),
        }
    }

    fn init(&mut self) {
        self.head = ptr::null_mut();
        self.tail = ptr::null_mut();
        self.count.store(0, Ordering::Relaxed);
    }

    fn clear_with_ref_release(&mut self) {
        let mut cursor = self.head;
        while !cursor.is_null() {
            let next = unsafe { (*cursor).next_ready };
            unsafe {
                (*cursor).next_ready = ptr::null_mut();
                (*cursor).dec_ref();
            }
            cursor = next;
        }
        self.init();
    }

    fn is_empty(&self) -> bool {
        self.count.load(Ordering::Relaxed) == 0
    }

    fn len(&self) -> u32 {
        self.count.load(Ordering::Relaxed)
    }

    fn contains(&self, task: *mut Task) -> bool {
        let mut cursor = self.head;
        while !cursor.is_null() {
            if cursor == task {
                return true;
            }
            cursor = unsafe { (*cursor).next_ready };
        }
        false
    }

    fn enqueue(&mut self, task: *mut Task) -> i32 {
        if task.is_null() {
            return -1;
        }
        if self.contains(task) {
            return 0;
        }
        unsafe { (*task).next_ready = ptr::null_mut() };
        if self.head.is_null() {
            self.head = task;
            self.tail = task;
        } else {
            unsafe { (*self.tail).next_ready = task };
            self.tail = task;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        unsafe {
            (*task).inc_ref();
        }
        0
    }

    fn dequeue(&mut self) -> *mut Task {
        if self.is_empty() {
            return ptr::null_mut();
        }
        let task = self.head;
        unsafe {
            self.head = (*task).next_ready;
            if self.head.is_null() {
                self.tail = ptr::null_mut();
            }
            (*task).next_ready = ptr::null_mut();
        }
        self.count.fetch_sub(1, Ordering::Relaxed);
        unsafe {
            (*task).dec_ref();
        }
        task
    }

    fn remove(&mut self, task: *mut Task) -> i32 {
        if task.is_null() || self.is_empty() {
            return -1;
        }
        let mut prev: *mut Task = ptr::null_mut();
        let mut cursor = self.head;
        while !cursor.is_null() {
            if cursor == task {
                if !prev.is_null() {
                    unsafe { (*prev).next_ready = (*cursor).next_ready };
                } else {
                    self.head = unsafe { (*cursor).next_ready };
                }
                if self.tail == cursor {
                    self.tail = prev;
                }
                unsafe { (*cursor).next_ready = ptr::null_mut() };
                self.count.fetch_sub(1, Ordering::Relaxed);
                unsafe {
                    (*cursor).dec_ref();
                }
                return 0;
            }
            prev = cursor;
            cursor = unsafe { (*cursor).next_ready };
        }
        -1
    }

    fn steal_from_tail(&mut self) -> Option<*mut Task> {
        if self.count.load(Ordering::Relaxed) <= 1 {
            return None;
        }

        let mut prev: *mut Task = ptr::null_mut();
        let mut cursor = self.head;

        while !cursor.is_null() {
            let next = unsafe { (*cursor).next_ready };
            if next.is_null() {
                break;
            }
            prev = cursor;
            cursor = next;
        }

        if cursor.is_null() || prev.is_null() {
            return None;
        }

        unsafe { (*prev).next_ready = ptr::null_mut() };
        self.tail = prev;
        self.count.fetch_sub(1, Ordering::Relaxed);
        unsafe {
            (*cursor).dec_ref();
        }

        Some(cursor)
    }
}

const EMPTY_QUEUE: ReadyQueue = ReadyQueue::new();

#[repr(C, align(64))]
pub struct PerCpuScheduler {
    pub cpu_id: usize,
    ready_queues: UnsafeCell<[ReadyQueue; NUM_PRIORITY_LEVELS]>,
    queue_lock: IrqMutex<()>,
    current_task_atomic: AtomicPtr<Task>,
    idle_task_atomic: AtomicPtr<Task>,
    pub enabled: AtomicBool,
    pub time_slice: u16,
    pub total_switches: AtomicU64,
    pub total_preemptions: AtomicU64,
    pub total_ticks: AtomicU64,
    pub idle_time: AtomicU64,
    pub total_yields: AtomicU64,
    pub schedule_calls: AtomicU32,
    initialized: AtomicBool,
    pub return_context: UnsafeCell<TaskContext>,
    executing_task: AtomicBool,
    remote_inbox_head: AtomicPtr<Task>,
    inbox_count: AtomicU32,
}

unsafe impl Send for PerCpuScheduler {}
unsafe impl Sync for PerCpuScheduler {}

impl PerCpuScheduler {
    pub const fn new() -> Self {
        Self {
            cpu_id: 0,
            ready_queues: UnsafeCell::new([EMPTY_QUEUE; NUM_PRIORITY_LEVELS]),
            queue_lock: IrqMutex::new(()),
            current_task_atomic: AtomicPtr::new(ptr::null_mut()),
            idle_task_atomic: AtomicPtr::new(ptr::null_mut()),
            enabled: AtomicBool::new(false),
            time_slice: 10,
            total_switches: AtomicU64::new(0),
            total_preemptions: AtomicU64::new(0),
            total_ticks: AtomicU64::new(0),
            idle_time: AtomicU64::new(0),
            total_yields: AtomicU64::new(0),
            schedule_calls: AtomicU32::new(0),
            initialized: AtomicBool::new(false),
            return_context: UnsafeCell::new(TaskContext::zero()),
            executing_task: AtomicBool::new(false),
            remote_inbox_head: AtomicPtr::new(ptr::null_mut()),
            inbox_count: AtomicU32::new(0),
        }
    }

    pub fn set_executing_task(&self, executing: bool) {
        self.executing_task.store(executing, Ordering::SeqCst);
    }

    pub fn is_executing_task(&self) -> bool {
        self.executing_task.load(Ordering::SeqCst)
    }

    #[inline]
    pub fn current_task(&self) -> *mut Task {
        self.current_task_atomic.load(Ordering::Acquire)
    }

    #[inline]
    pub fn set_current_task(&self, task: *mut Task) {
        self.current_task_atomic.store(task, Ordering::Release);
    }

    #[inline]
    pub fn idle_task(&self) -> *mut Task {
        self.idle_task_atomic.load(Ordering::Acquire)
    }

    #[inline]
    pub fn set_idle_task_atomic(&self, task: *mut Task) {
        self.idle_task_atomic.store(task, Ordering::Release);
    }

    /// # Safety
    /// Must be called exactly once per CPU during single-threaded init.
    pub unsafe fn init(&self, cpu_id: usize) {
        // SAFETY: called during single-threaded init before any concurrent access.
        // cpu_id and time_slice are plain fields written only here; use raw pointer.
        unsafe {
            let ptr = self as *const Self as *mut Self;
            (*ptr).cpu_id = cpu_id;
            (*ptr).time_slice = 10;
            let queues = &mut *self.ready_queues.get();
            for queue in queues.iter_mut() {
                queue.clear_with_ref_release();
            }
        }
        self.current_task_atomic
            .store(ptr::null_mut(), Ordering::Release);
        if !self.is_initialized() {
            self.idle_task_atomic
                .store(ptr::null_mut(), Ordering::Release);
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
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn enqueue_local(&self, task: *mut Task) -> i32 {
        if task.is_null() {
            return -1;
        }
        let self_addr = self as *const _ as usize;
        if self_addr < 0xffffffff80000000 {
            klog_info!(
                "SCHED: BUG - enqueue_local called with invalid self=0x{:x}",
                self_addr
            );
            return -1;
        }
        let priority = unsafe { (*task).priority as usize };
        let idx = priority.min(NUM_PRIORITY_LEVELS - 1);

        unsafe {
            (*task).last_cpu = self.cpu_id as u8;
        }

        let _guard = self.queue_lock.lock();
        // SAFETY: queue_lock held, exclusive access to ready_queues
        let queues = unsafe { &mut *self.ready_queues.get() };
        queues[idx].enqueue(task)
    }

    pub fn dequeue_highest_priority(&self) -> *mut Task {
        let self_addr = self as *const _ as usize;
        if self_addr < 0xffffffff80000000 {
            klog_info!(
                "SCHED: BUG - dequeue_highest_priority called with invalid self=0x{:x}",
                self_addr
            );
            return ptr::null_mut();
        }
        let _guard = self.queue_lock.lock();
        // SAFETY: queue_lock held, exclusive access to ready_queues
        let queues = unsafe { &mut *self.ready_queues.get() };
        for queue in queues.iter_mut() {
            let task = queue.dequeue();
            if !task.is_null() {
                return task;
            }
        }
        ptr::null_mut()
    }

    pub fn remove_task(&self, task: *mut Task) -> i32 {
        if task.is_null() {
            return -1;
        }
        let priority = unsafe { (*task).priority as usize };
        let idx = priority.min(NUM_PRIORITY_LEVELS - 1);
        let _guard = self.queue_lock.lock();
        // SAFETY: queue_lock held, exclusive access to ready_queues
        let queues = unsafe { &mut *self.ready_queues.get() };
        queues[idx].remove(task)
    }

    pub fn total_ready_count(&self) -> u32 {
        let _guard = self.queue_lock.lock();
        // SAFETY: queue_lock held, read-only access to ready_queues
        let queues = unsafe { &*self.ready_queues.get() };
        queues.iter().map(|q| q.len()).sum()
    }

    /// Lock-free approximate idle check.  Reads each priority queue's atomic
    /// count without acquiring the queue lock.  May briefly race with a
    /// concurrent enqueue/dequeue, but false positives are benign — the task
    /// will be dispatched on the next scheduling pass.  This is the fast path
    /// used by `select_target_cpu()` to avoid locking every candidate CPU.
    pub fn is_idle_approx(&self) -> bool {
        // SAFETY: ReadyQueue counts are AtomicU32 — safe to read without the lock.
        let queues = unsafe { &*self.ready_queues.get() };
        queues.iter().all(|q| q.is_empty())
    }

    /// Returns the effective load on this CPU: queued tasks plus one if a
    /// non-idle task is currently running.  Lock-free and approximate.
    /// Mirrors Linux's `rq->nr_running` which includes the running task.
    pub fn effective_load(&self) -> u32 {
        let queues = unsafe { &*self.ready_queues.get() };
        let queued: u32 = queues.iter().map(|q| q.len()).sum();
        let current = self.current_task_atomic.load(Ordering::Relaxed);
        let idle = self.idle_task_atomic.load(Ordering::Relaxed);
        let running_real = !current.is_null() && current != idle;
        if running_real { queued + 1 } else { queued }
    }

    pub fn steal_task(&self) -> Option<*mut Task> {
        let _guard = self.queue_lock.lock();
        // SAFETY: queue_lock held, exclusive access to ready_queues
        let queues = unsafe { &mut *self.ready_queues.get() };
        for queue in queues.iter_mut().rev() {
            if let Some(task) = queue.steal_from_tail() {
                return Some(task);
            }
        }
        None
    }

    pub fn set_idle_task(&self, task: *mut Task) {
        self.idle_task_atomic.store(task, Ordering::Release);
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
    pub fn push_remote_wake(&self, task: *mut Task) {
        if task.is_null() {
            return;
        }

        // Acquire inbox ownership before publishing task into the lock-free list.
        // This prevents a drain from observing the task and dropping the reference
        // before the producer has incremented refcnt.
        unsafe {
            (*task).last_cpu = self.cpu_id as u8;
            (*task).inc_ref();
        }

        // Lock-free push using CAS loop (Treiber stack pattern)
        loop {
            // Load current head
            let old_head = self.remote_inbox_head.load(Ordering::Acquire);

            // Point our next to current head
            unsafe {
                (*task).next_inbox.store(old_head, Ordering::Relaxed);
            }

            // Try to become new head
            match self.remote_inbox_head.compare_exchange_weak(
                old_head,
                task,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Success! Update count and return
                    self.inbox_count.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => {
                    // Lost race - retry
                    core::hint::spin_loop();
                }
            }
        }
    }

    /// Drain all tasks from remote inbox into local ready queues.
    /// MUST only be called by the owning CPU.
    pub fn drain_remote_inbox(&self) {
        let head = self
            .remote_inbox_head
            .swap(ptr::null_mut(), Ordering::AcqRel);

        if head.is_null() {
            return;
        }

        let mut count = 0u32;
        let mut current = head;

        let mut reversed: *mut Task = ptr::null_mut();
        while !current.is_null() {
            let next = unsafe { (*current).next_inbox.load(Ordering::Acquire) };
            unsafe {
                (*current).next_inbox.store(reversed, Ordering::Relaxed);
            }
            reversed = current;
            current = next;
            count += 1;
        }

        current = reversed;
        while !current.is_null() {
            let next = unsafe { (*current).next_inbox.load(Ordering::Acquire) };

            unsafe {
                (*current)
                    .next_inbox
                    .store(ptr::null_mut(), Ordering::Release);
            }

            let should_enqueue = unsafe { (*current).status() == TaskStatus::Ready };
            if should_enqueue {
                unsafe {
                    (*current).last_cpu = self.cpu_id as u8;
                }
                let priority = unsafe { (*current).priority as usize };
                let idx = priority.min(NUM_PRIORITY_LEVELS - 1);

                let _guard = self.queue_lock.lock();
                // SAFETY: queue_lock held
                let queues = unsafe { &mut *self.ready_queues.get() };
                queues[idx].enqueue(current);
                drop(_guard);
            }

            unsafe {
                (*current).dec_ref();
            }

            current = next;
        }

        self.saturating_sub_inbox_count(count);
    }

    fn clear_remote_inbox_with_ref_release(&self) {
        let mut cursor = self
            .remote_inbox_head
            .swap(ptr::null_mut(), Ordering::AcqRel);
        let mut drained = 0u32;
        while !cursor.is_null() {
            let next = unsafe { (*cursor).next_inbox.load(Ordering::Acquire) };
            unsafe {
                (*cursor)
                    .next_inbox
                    .store(ptr::null_mut(), Ordering::Release);
                (*cursor).dec_ref();
            }
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

static CPU_SCHEDULERS: SyncUnsafeCell<[PerCpuScheduler; MAX_CPUS]> = SyncUnsafeCell::new({
    const INIT: PerCpuScheduler = PerCpuScheduler::new();
    [INIT; MAX_CPUS]
});

static SCHEDULERS_INIT: InitFlag = InitFlag::new();

pub fn init_percpu_scheduler(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    unsafe {
        (*CPU_SCHEDULERS.get())[cpu_id].init(cpu_id);
    }
    klog_debug!("SCHED: Per-CPU scheduler initialized for CPU {}", cpu_id);
}

pub fn init_all_percpu_schedulers() {
    if !SCHEDULERS_INIT.init_once() {
        return;
    }

    for cpu_id in 0..MAX_CPUS {
        unsafe {
            (*CPU_SCHEDULERS.get())[cpu_id].init(cpu_id);
        }
    }
}

pub fn is_percpu_scheduler_initialized(cpu_id: usize) -> bool {
    if cpu_id >= MAX_CPUS {
        return false;
    }
    unsafe { (*CPU_SCHEDULERS.get())[cpu_id].is_initialized() }
}

pub fn with_local_scheduler<R>(f: impl FnOnce(&PerCpuScheduler) -> R) -> R {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    // SAFETY: cpu_id < MAX_CPUS guaranteed by get_current_cpu; shared ref only
    let sched = unsafe { &(*CPU_SCHEDULERS.get())[cpu_id] };
    f(sched)
}

pub fn with_cpu_scheduler<R>(cpu_id: usize, f: impl FnOnce(&PerCpuScheduler) -> R) -> Option<R> {
    if cpu_id >= MAX_CPUS {
        return None;
    }
    // SAFETY: bounds checked; shared ref only — interior mutability handles mutation
    let sched = unsafe { &(*CPU_SCHEDULERS.get())[cpu_id] };
    if !sched.is_initialized() {
        return None;
    }
    Some(f(sched))
}

pub fn enqueue_task_on_cpu(cpu_id: usize, task: *mut Task) -> i32 {
    if cpu_id >= MAX_CPUS || task.is_null() {
        return -1;
    }

    unsafe {
        if (*task).status() != TaskStatus::Ready {
            return -1;
        }
    }

    with_cpu_scheduler(cpu_id, |sched| sched.enqueue_local(task)).unwrap_or(-1)
}

pub fn try_steal_task_from_cpu(cpu_id: usize) -> Option<*mut Task> {
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
pub fn select_target_cpu(task: *mut Task) -> Option<usize> {
    let current_cpu = slopos_arch::pcr::get_current_cpu();
    if task.is_null() {
        return if is_schedulable_cpu(current_cpu, 0)
            || is_local_enqueue_fallback_cpu(current_cpu, 0)
        {
            Some(current_cpu)
        } else {
            find_least_loaded_cpu(0)
        };
    }

    let affinity = unsafe { (*task).cpu_affinity };
    let last_cpu = unsafe { (*task).last_cpu as usize };

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

    None
}

/// Select the best CPU for a **newly created** task (fork, spawn, exec).
///
/// Mirrors Linux's `WF_FORK` / `SD_BALANCE_FORK` slow path: bypasses
/// `last_cpu` entirely (cache is cold for a new address space) and finds
/// the globally idlest CPU.  A round-robin counter rotates the scan start
/// so sequential forks spread evenly when all CPUs have equal load.
pub fn select_target_cpu_for_new(task: *mut Task) -> Option<usize> {
    let current_cpu = slopos_arch::pcr::get_current_cpu();
    if task.is_null() {
        return if is_schedulable_cpu(current_cpu, 0)
            || is_local_enqueue_fallback_cpu(current_cpu, 0)
        {
            Some(current_cpu)
        } else {
            find_idlest_cpu(0)
        };
    }

    let affinity = unsafe { (*task).cpu_affinity };

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
fn find_idlest_cpu(affinity: u32) -> Option<usize> {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    if cpu_count == 0 {
        return None;
    }

    // Rotate start position so sequential calls spread across CPUs.
    let start = FORK_RR_COUNTER.fetch_add(1, Ordering::Relaxed) % cpu_count;

    let mut best_cpu: Option<usize> = None;
    let mut min_load = u32::MAX;

    for i in 0..cpu_count {
        let cpu_id = (start + i) % cpu_count;
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
pub fn get_ap_return_context(cpu_id: usize) -> *mut TaskContext {
    if cpu_id >= MAX_CPUS {
        return ptr::null_mut();
    }
    // SAFETY: return_context is only written during single-threaded init
    // and read by the owning CPU
    unsafe { (*CPU_SCHEDULERS.get())[cpu_id].return_context.get() }
}

/// Check if the given task is the idle task for any CPU
pub fn is_idle_task(task: *const Task) -> bool {
    if task.is_null() {
        return false;
    }

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        if let Some(is_idle) =
            with_cpu_scheduler(cpu_id, |sched| sched.idle_task() == task as *mut Task)
        {
            if is_idle {
                return true;
            }
        }
    }

    false
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
        if let Some(count) = with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()) {
            if count > 0 {
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
    // SAFETY: bounds checked; interior mutability via queue_lock + UnsafeCell
    let sched = unsafe { &(*CPU_SCHEDULERS.get())[cpu_id] };
    let _guard = sched.queue_lock.lock();
    // SAFETY: queue_lock held
    let queues = unsafe { &mut *sched.ready_queues.get() };
    for queue in queues.iter_mut() {
        queue.clear_with_ref_release();
    }
    drop(_guard);
    sched.clear_remote_inbox_with_ref_release();
    sched
        .current_task_atomic
        .store(ptr::null_mut(), Ordering::Release);
}

/// Clear all per-CPU ready queues across all CPUs. Used during scheduler shutdown.
pub fn clear_all_cpu_queues() {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        clear_cpu_queues(cpu_id);
    }
}
