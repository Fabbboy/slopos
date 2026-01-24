//! Per-CPU Scheduler for SMP Support
//!
//! Each CPU has its own scheduler instance with local run queues.
//! This minimizes lock contention and improves cache locality.

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use slopos_abi::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_PRIORITY_IDLE, TASK_STATE_READY, Task, TaskContext,
};
use slopos_lib::{MAX_CPUS, klog_debug, klog_info};
use spin::Mutex;

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
                return 0;
            }
            prev = cursor;
            cursor = unsafe { (*cursor).next_ready };
        }
        -1
    }

    #[allow(dead_code)]
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

        Some(cursor)
    }
}

const EMPTY_QUEUE: ReadyQueue = ReadyQueue::new();

#[repr(C, align(64))]
pub struct PerCpuScheduler {
    pub cpu_id: usize,
    ready_queues: [ReadyQueue; NUM_PRIORITY_LEVELS],
    queue_lock: Mutex<()>,
    current_task_atomic: AtomicPtr<Task>,
    idle_task_atomic: AtomicPtr<Task>,
    pub enabled: AtomicBool,
    pub time_slice: u16,
    pub total_switches: AtomicU64,
    pub total_preemptions: AtomicU64,
    pub total_ticks: AtomicU64,
    pub idle_time: AtomicU64,
    initialized: AtomicBool,
    pub return_context: TaskContext,
    executing_task: AtomicBool,
}

unsafe impl Send for PerCpuScheduler {}
unsafe impl Sync for PerCpuScheduler {}

impl PerCpuScheduler {
    pub const fn new() -> Self {
        Self {
            cpu_id: 0,
            ready_queues: [EMPTY_QUEUE; NUM_PRIORITY_LEVELS],
            queue_lock: Mutex::new(()),
            current_task_atomic: AtomicPtr::new(ptr::null_mut()),
            idle_task_atomic: AtomicPtr::new(ptr::null_mut()),
            enabled: AtomicBool::new(false),
            time_slice: 10,
            total_switches: AtomicU64::new(0),
            total_preemptions: AtomicU64::new(0),
            total_ticks: AtomicU64::new(0),
            idle_time: AtomicU64::new(0),
            initialized: AtomicBool::new(false),
            return_context: TaskContext::zero(),
            executing_task: AtomicBool::new(false),
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

    pub fn init(&mut self, cpu_id: usize) {
        self.cpu_id = cpu_id;
        for queue in self.ready_queues.iter_mut() {
            queue.init();
        }
        self.current_task_atomic
            .store(ptr::null_mut(), Ordering::Release);
        if !self.is_initialized() {
            self.idle_task_atomic
                .store(ptr::null_mut(), Ordering::Release);
        }
        self.enabled.store(false, Ordering::Relaxed);
        self.time_slice = 10;
        self.total_switches.store(0, Ordering::Relaxed);
        self.total_preemptions.store(0, Ordering::Relaxed);
        self.total_ticks.store(0, Ordering::Relaxed);
        self.idle_time.store(0, Ordering::Relaxed);
        self.initialized.store(true, Ordering::Release);
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn enqueue_local(&mut self, task: *mut Task) -> i32 {
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
        self.ready_queues[idx].enqueue(task)
    }

    pub fn dequeue_highest_priority(&mut self) -> *mut Task {
        // Sanity check: ensure self pointer is in valid kernel space
        let self_addr = self as *const _ as usize;
        if self_addr < 0xffffffff80000000 {
            klog_info!(
                "SCHED: BUG - dequeue_highest_priority called with invalid self=0x{:x}",
                self_addr
            );
            return ptr::null_mut();
        }
        let _guard = self.queue_lock.lock();
        for queue in self.ready_queues.iter_mut() {
            let task = queue.dequeue();
            if !task.is_null() {
                return task;
            }
        }
        ptr::null_mut()
    }

    pub fn remove_task(&mut self, task: *mut Task) -> i32 {
        if task.is_null() {
            return -1;
        }
        let priority = unsafe { (*task).priority as usize };
        let idx = priority.min(NUM_PRIORITY_LEVELS - 1);
        let _guard = self.queue_lock.lock();
        self.ready_queues[idx].remove(task)
    }

    pub fn total_ready_count(&self) -> u32 {
        let _guard = self.queue_lock.lock();
        self.ready_queues.iter().map(|q| q.len()).sum()
    }

    #[allow(dead_code)]
    pub fn steal_task(&mut self) -> Option<*mut Task> {
        let _guard = self.queue_lock.lock();
        for queue in self.ready_queues.iter_mut().rev() {
            if let Some(task) = queue.steal_from_tail() {
                return Some(task);
            }
        }
        None
    }

    pub fn set_idle_task(&mut self, task: *mut Task) {
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
}

static mut CPU_SCHEDULERS: [PerCpuScheduler; MAX_CPUS] = {
    const INIT: PerCpuScheduler = PerCpuScheduler::new();
    [INIT; MAX_CPUS]
};

static SCHEDULERS_INITIALIZED: AtomicBool = AtomicBool::new(false);

pub fn init_percpu_scheduler(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    unsafe {
        CPU_SCHEDULERS[cpu_id].init(cpu_id);
    }
    klog_debug!("SCHED: Per-CPU scheduler initialized for CPU {}", cpu_id);
}

pub fn init_all_percpu_schedulers() {
    for cpu_id in 0..MAX_CPUS {
        unsafe {
            CPU_SCHEDULERS[cpu_id].init(cpu_id);
        }
    }
    SCHEDULERS_INITIALIZED.store(true, Ordering::Release);
}

pub fn is_percpu_scheduler_initialized(cpu_id: usize) -> bool {
    if cpu_id >= MAX_CPUS {
        return false;
    }
    unsafe { CPU_SCHEDULERS[cpu_id].is_initialized() }
}

pub fn with_local_scheduler<R>(f: impl FnOnce(&mut PerCpuScheduler) -> R) -> R {
    let cpu_id = slopos_lib::get_current_cpu();
    unsafe {
        let sched = &mut CPU_SCHEDULERS[cpu_id];
        f(sched)
    }
}

pub fn with_cpu_scheduler<R>(
    cpu_id: usize,
    f: impl FnOnce(&mut PerCpuScheduler) -> R,
) -> Option<R> {
    if cpu_id >= MAX_CPUS {
        return None;
    }
    unsafe {
        let sched = &mut CPU_SCHEDULERS[cpu_id];
        if !sched.is_initialized() {
            return None;
        }
        Some(f(sched))
    }
}

pub fn get_local_scheduler() -> &'static mut PerCpuScheduler {
    let cpu_id = slopos_lib::get_current_cpu();
    unsafe { &mut CPU_SCHEDULERS[cpu_id] }
}

pub fn get_cpu_scheduler(cpu_id: usize) -> Option<&'static mut PerCpuScheduler> {
    if cpu_id >= MAX_CPUS {
        return None;
    }
    unsafe {
        let sched = &mut CPU_SCHEDULERS[cpu_id];
        if sched.is_initialized() {
            Some(sched)
        } else {
            None
        }
    }
}

pub fn enqueue_task_on_cpu(cpu_id: usize, task: *mut Task) -> i32 {
    if cpu_id >= MAX_CPUS || task.is_null() {
        return -1;
    }

    unsafe {
        if (*task).state != TASK_STATE_READY {
            return -1;
        }
    }

    with_cpu_scheduler(cpu_id, |sched| sched.enqueue_local(task)).unwrap_or(-1)
}

pub fn get_total_ready_tasks() -> u32 {
    let mut total = 0u32;
    let cpu_count = slopos_lib::get_cpu_count();
    for cpu_id in 0..cpu_count {
        if let Some(count) = with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()) {
            total += count;
        }
    }
    total
}

pub fn select_target_cpu(task: *mut Task) -> usize {
    if task.is_null() {
        return slopos_lib::get_current_cpu();
    }

    let affinity = unsafe { (*task).cpu_affinity };
    let last_cpu = unsafe { (*task).last_cpu as usize };
    let cpu_count = slopos_lib::get_cpu_count();

    if affinity != 0 && (affinity & (1 << last_cpu)) != 0 && last_cpu < cpu_count {
        if is_percpu_scheduler_initialized(last_cpu) {
            return last_cpu;
        }
    }

    find_least_loaded_cpu(affinity)
}

fn find_least_loaded_cpu(affinity: u32) -> usize {
    let cpu_count = slopos_lib::get_cpu_count();
    let mut best_cpu = 0usize;
    let mut min_load = u32::MAX;

    for cpu_id in 0..cpu_count {
        if affinity != 0 && (affinity & (1 << cpu_id)) == 0 {
            continue;
        }

        if !is_percpu_scheduler_initialized(cpu_id) {
            continue;
        }

        if let Some(load) = with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()) {
            if load < min_load {
                min_load = load;
                best_cpu = cpu_id;
            }
        }
    }

    best_cpu
}

// =============================================================================
// Per-CPU Idle Task Creation for SMP
// =============================================================================

/// Idle loop function for AP idle tasks.
/// This is the entry point for each AP's idle task.
fn ap_idle_loop(_: *mut c_void) {
    loop {
        // Increment idle time counter
        let cpu_id = slopos_lib::get_current_cpu();
        with_cpu_scheduler(cpu_id, |sched| {
            sched.increment_idle_time();
        });
        // Wait for interrupt (reschedule IPI or timer). The sti; hlt sequence
        // atomically enables interrupts and halts until the next interrupt.
        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
    }
}

/// Create an idle task specifically for an Application Processor.
/// Returns the task pointer on success, null on failure.
///
/// This creates a minimal kernel task that will serve as the "from" context
/// when the AP picks up real work from its queue.
pub fn create_ap_idle_task(cpu_id: usize) -> *mut Task {
    use crate::task::{task_create, task_get_info};

    if cpu_id == 0 {
        klog_info!("SCHED: CPU 0 should use create_idle_task(), not create_ap_idle_task()");
        return ptr::null_mut();
    }

    if cpu_id >= MAX_CPUS {
        klog_info!("SCHED: Invalid CPU ID {} for AP idle task", cpu_id);
        return ptr::null_mut();
    }

    // Create a unique name for this CPU's idle task
    let mut name = [0u8; 16];
    let prefix = b"ap_idle_";
    name[..prefix.len()].copy_from_slice(prefix);
    // Add CPU number (simple digit conversion for cpu_id < 10)
    if cpu_id < 10 {
        name[prefix.len()] = b'0' + cpu_id as u8;
        name[prefix.len() + 1] = 0;
    } else {
        name[prefix.len()] = b'0' + (cpu_id / 10) as u8;
        name[prefix.len() + 1] = b'0' + (cpu_id % 10) as u8;
        name[prefix.len() + 2] = 0;
    }

    let task_id = unsafe {
        task_create(
            name.as_ptr() as *const i8,
            core::mem::transmute(ap_idle_loop as *const ()),
            ptr::null_mut(),
            TASK_PRIORITY_IDLE,
            TASK_FLAG_KERNEL_MODE,
        )
    };

    if task_id == INVALID_TASK_ID {
        klog_info!("SCHED: Failed to create idle task for CPU {}", cpu_id);
        return ptr::null_mut();
    }

    let mut idle_task: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut idle_task) != 0 || idle_task.is_null() {
        klog_info!("SCHED: Failed to get idle task info for CPU {}", cpu_id);
        return ptr::null_mut();
    }

    // Set CPU affinity to only run on this specific CPU
    unsafe {
        (*idle_task).cpu_affinity = 1 << cpu_id;
        (*idle_task).last_cpu = cpu_id as u8;
    }

    // Register with per-CPU scheduler
    with_cpu_scheduler(cpu_id, |sched| {
        sched.set_idle_task(idle_task);
    });

    klog_debug!("SCHED: Created idle task {} for CPU {}", task_id, cpu_id);

    idle_task
}

/// Get the return context for an AP to use when no tasks are available.
/// This is stored in the per-CPU scheduler and initialized during AP startup.
pub fn get_ap_return_context(cpu_id: usize) -> *mut TaskContext {
    unsafe {
        if cpu_id >= MAX_CPUS {
            return ptr::null_mut();
        }
        &raw mut CPU_SCHEDULERS[cpu_id].return_context
    }
}

/// Check if the given task is the idle task for any CPU
pub fn is_idle_task(task: *const Task) -> bool {
    if task.is_null() {
        return false;
    }

    let cpu_count = slopos_lib::get_cpu_count();
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

/// Pause all AP scheduler loops. Call this before clearing queues/reinitializing.
/// Returns true if APs were already paused (caller should NOT resume in that case).
pub fn pause_all_aps() -> bool {
    let was_paused = AP_PAUSED.swap(true, Ordering::SeqCst);
    if !was_paused {
        core::sync::atomic::fence(Ordering::SeqCst);
        let cpu_count = slopos_lib::get_cpu_count();
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

/// Resume all AP scheduler loops after reinitialization is complete.
pub fn resume_all_aps() {
    core::sync::atomic::fence(Ordering::SeqCst);
    AP_PAUSED.store(false, Ordering::SeqCst);
}

/// Conditionally resume APs only if they weren't already paused before our pause call.
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

/// Clear all ready queues for a specific CPU. Used during test reinitialization.
pub fn clear_cpu_queues(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    unsafe {
        let sched = &mut CPU_SCHEDULERS[cpu_id];
        let _guard = sched.queue_lock.lock();
        for queue in sched.ready_queues.iter_mut() {
            queue.init();
        }
        // Clear current task but preserve idle task
        sched
            .current_task_atomic
            .store(ptr::null_mut(), Ordering::Release);
    }
}

/// Clear all per-CPU ready queues across all CPUs. Used during scheduler shutdown.
pub fn clear_all_cpu_queues() {
    let cpu_count = slopos_lib::get_cpu_count();
    for cpu_id in 0..cpu_count {
        clear_cpu_queues(cpu_id);
    }
}
