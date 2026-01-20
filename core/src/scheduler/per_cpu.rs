//! Per-CPU Scheduler for SMP Support
//!
//! Each CPU has its own scheduler instance with local run queues.
//! This minimizes lock contention and improves cache locality.

use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use slopos_abi::task::{TASK_STATE_READY, Task};
use slopos_lib::{MAX_CPUS, klog_debug};

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
    pub current_task: *mut Task,
    pub idle_task: *mut Task,
    pub enabled: AtomicBool,
    pub time_slice: u16,
    pub total_switches: AtomicU64,
    pub total_preemptions: AtomicU64,
    pub total_ticks: AtomicU64,
    pub idle_time: AtomicU64,
    initialized: AtomicBool,
}

unsafe impl Send for PerCpuScheduler {}
unsafe impl Sync for PerCpuScheduler {}

impl PerCpuScheduler {
    pub const fn new() -> Self {
        Self {
            cpu_id: 0,
            ready_queues: [EMPTY_QUEUE; NUM_PRIORITY_LEVELS],
            current_task: ptr::null_mut(),
            idle_task: ptr::null_mut(),
            enabled: AtomicBool::new(false),
            time_slice: 10,
            total_switches: AtomicU64::new(0),
            total_preemptions: AtomicU64::new(0),
            total_ticks: AtomicU64::new(0),
            idle_time: AtomicU64::new(0),
            initialized: AtomicBool::new(false),
        }
    }

    pub fn init(&mut self, cpu_id: usize) {
        self.cpu_id = cpu_id;
        for queue in self.ready_queues.iter_mut() {
            queue.init();
        }
        self.current_task = ptr::null_mut();
        self.idle_task = ptr::null_mut();
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
        let priority = unsafe { (*task).priority as usize };
        let idx = priority.min(NUM_PRIORITY_LEVELS - 1);

        unsafe {
            (*task).last_cpu = self.cpu_id as u8;
        }

        self.ready_queues[idx].enqueue(task)
    }

    pub fn dequeue_highest_priority(&mut self) -> *mut Task {
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
        self.ready_queues[idx].remove(task)
    }

    pub fn total_ready_count(&self) -> u32 {
        self.ready_queues.iter().map(|q| q.len()).sum()
    }

    #[allow(dead_code)]
    pub fn steal_task(&mut self) -> Option<*mut Task> {
        for queue in self.ready_queues.iter_mut().rev() {
            if let Some(task) = queue.steal_from_tail() {
                return Some(task);
            }
        }
        None
    }

    pub fn set_idle_task(&mut self, task: *mut Task) {
        self.idle_task = task;
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
