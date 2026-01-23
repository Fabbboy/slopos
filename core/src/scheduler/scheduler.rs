use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_lib::preempt::PreemptGuard;
use slopos_lib::IrqMutex;
use spin::Once;

use slopos_lib::kdiag_timestamp;
use slopos_lib::klog_info;

use crate::platform;
use crate::wl_currency;

use super::per_cpu;
use super::task::{
    task_get_info, task_get_state, task_is_blocked, task_is_ready, task_is_running,
    task_is_terminated, task_record_context_switch, task_record_yield, task_set_current,
    task_set_state, Task, TaskContext, INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE,
    TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE, TASK_PRIORITY_IDLE, TASK_STATE_BLOCKED,
    TASK_STATE_READY, TASK_STATE_RUNNING,
};

const SCHED_DEFAULT_TIME_SLICE: u32 = 10;
const SCHED_POLICY_COOPERATIVE: u8 = 2;
const SCHEDULER_PREEMPTION_DEFAULT: u8 = 1;

const NUM_PRIORITY_LEVELS: usize = 4;

#[derive(Default)]
struct ReadyQueue {
    head: *mut Task,
    tail: *mut Task,
    count: u32,
}

// SAFETY: ReadyQueue contains raw pointers to Task in static storage.
// Access serialized through SchedulerInner mutex.
unsafe impl Send for ReadyQueue {}

struct SchedulerInner {
    ready_queues: [ReadyQueue; NUM_PRIORITY_LEVELS],
    current_task: *mut Task,
    idle_task: *mut Task,
    policy: u8,
    enabled: u8,
    time_slice: u16,
    return_context: TaskContext,
    total_switches: u64,
    total_yields: u64,
    idle_time: u64,
    total_ticks: u64,
    total_preemptions: u64,
    schedule_calls: u32,
    preemption_enabled: u8,
}

// SAFETY: SchedulerInner contains raw pointers to Task in static storage.
// All access serialized through mutex.
unsafe impl Send for SchedulerInner {}

const EMPTY_QUEUE: ReadyQueue = ReadyQueue {
    head: ptr::null_mut(),
    tail: ptr::null_mut(),
    count: 0,
};

impl SchedulerInner {
    const fn new() -> Self {
        Self {
            ready_queues: [EMPTY_QUEUE; NUM_PRIORITY_LEVELS],
            current_task: ptr::null_mut(),
            idle_task: ptr::null_mut(),
            policy: SCHED_POLICY_COOPERATIVE,
            enabled: 0,
            time_slice: SCHED_DEFAULT_TIME_SLICE as u16,
            return_context: TaskContext {
                rax: 0,
                rbx: 0,
                rcx: 0,
                rdx: 0,
                rsi: 0,
                rdi: 0,
                rbp: 0,
                rsp: 0,
                r8: 0,
                r9: 0,
                r10: 0,
                r11: 0,
                r12: 0,
                r13: 0,
                r14: 0,
                r15: 0,
                rip: 0,
                rflags: 0,
                cs: 0,
                ds: 0,
                es: 0,
                fs: 0,
                gs: 0,
                ss: 0,
                cr3: 0,
            },
            total_switches: 0,
            total_yields: 0,
            idle_time: 0,
            total_ticks: 0,
            total_preemptions: 0,
            schedule_calls: 0,
            preemption_enabled: SCHEDULER_PREEMPTION_DEFAULT,
        }
    }

    fn total_ready_count(&self) -> u32 {
        self.ready_queues.iter().map(|q| q.count).sum()
    }

    fn enqueue_task(&mut self, task: *mut Task) -> c_int {
        if task.is_null() {
            return -1;
        }
        let priority = unsafe { (*task).priority as usize };
        let idx = priority.min(NUM_PRIORITY_LEVELS - 1);
        self.ready_queues[idx].enqueue(task)
    }

    fn dequeue_highest_priority(&mut self) -> *mut Task {
        for queue in self.ready_queues.iter_mut() {
            let task = queue.dequeue();
            if !task.is_null() {
                return task;
            }
        }
        ptr::null_mut()
    }

    fn remove_task(&mut self, task: *mut Task) -> c_int {
        if task.is_null() {
            return -1;
        }
        let priority = unsafe { (*task).priority as usize };
        let idx = priority.min(NUM_PRIORITY_LEVELS - 1);
        self.ready_queues[idx].remove(task)
    }

    fn init_queues(&mut self) {
        for queue in self.ready_queues.iter_mut() {
            queue.init();
        }
    }
}

static SCHEDULER: Once<IrqMutex<SchedulerInner>> = Once::new();
static IDLE_WAKEUP_CB: Once<IrqMutex<Option<fn() -> c_int>>> = Once::new();

#[inline]
fn with_scheduler<R>(f: impl FnOnce(&mut SchedulerInner) -> R) -> R {
    let mutex = SCHEDULER.get().expect("scheduler not initialized");
    let mut guard = mutex.lock();
    f(&mut guard)
}

#[inline]
fn try_with_scheduler<R>(f: impl FnOnce(&mut SchedulerInner) -> R) -> Option<R> {
    SCHEDULER.get().map(|mutex| {
        let mut guard = mutex.lock();
        f(&mut guard)
    })
}

use slopos_mm::paging::{paging_get_kernel_directory, paging_set_current_directory};
use slopos_mm::process_vm::process_vm_get_page_dir;
use slopos_mm::user_copy;

use super::ffi_boundary::{context_switch, context_switch_user, kernel_stack_top};

fn current_task_process_id() -> u32 {
    let task = scheduler_get_current_task();
    if task.is_null() {
        return crate::task::INVALID_PROCESS_ID;
    }
    unsafe { (*task).process_id }
}

impl ReadyQueue {
    fn init(&mut self) {
        self.head = ptr::null_mut();
        self.tail = ptr::null_mut();
        self.count = 0;
    }

    fn is_empty(&self) -> bool {
        self.count == 0
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

    fn enqueue(&mut self, task: *mut Task) -> c_int {
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
        self.count += 1;
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
        self.count -= 1;
        task
    }

    fn remove(&mut self, task: *mut Task) -> c_int {
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
                self.count -= 1;
                return 0;
            }
            prev = cursor;
            cursor = unsafe { (*cursor).next_ready };
        }
        -1
    }
}

fn get_default_time_slice(sched: &SchedulerInner) -> u64 {
    if sched.time_slice != 0 {
        sched.time_slice as u64
    } else {
        SCHED_DEFAULT_TIME_SLICE as u64
    }
}

fn reset_task_quantum(sched: &SchedulerInner, task: *mut Task) {
    if task.is_null() {
        return;
    }
    let slice = unsafe {
        if (*task).time_slice != 0 {
            (*task).time_slice
        } else {
            get_default_time_slice(sched)
        }
    };
    unsafe {
        (*task).time_slice = slice;
        (*task).time_slice_remaining = slice;
    }
}

pub fn clear_scheduler_current_task() {
    with_scheduler(|sched| {
        sched.current_task = ptr::null_mut();
    });
    task_set_current(ptr::null_mut());
}

pub fn schedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if !task_is_ready(task) {
        unsafe {
            klog_info!(
                "schedule_task: task {} not ready (state {})",
                (*task).task_id,
                task_get_state(task) as u32
            );
        }
        return -1;
    }

    with_scheduler(|sched| {
        if unsafe { (*task).time_slice_remaining } == 0 {
            reset_task_quantum(sched, task);
        }
    });

    let target_cpu = per_cpu::select_target_cpu(task);
    let current_cpu = slopos_lib::get_current_cpu();

    let result = per_cpu::with_cpu_scheduler(target_cpu, |sched| sched.enqueue_local(task));

    if result != Some(0) {
        with_scheduler(|sched| {
            if sched.enqueue_task(task) != 0 {
                klog_info!("schedule_task: all queues full, request rejected");
                return -1;
            }
            0
        })
    } else {
        // Memory barrier before potentially sending IPI to another CPU.
        // Ensures task state changes are visible to the target CPU.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        if target_cpu != current_cpu && slopos_lib::is_cpu_online(target_cpu) {
            send_reschedule_ipi(target_cpu);
        }
        0
    }
}

pub fn unschedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }

    let last_cpu = unsafe { (*task).last_cpu as usize };
    per_cpu::with_cpu_scheduler(last_cpu, |sched| {
        sched.remove_task(task);
        if sched.current_task() == task {
            sched.set_current_task(ptr::null_mut());
        }
    });

    with_scheduler(|sched| {
        sched.remove_task(task);
        if sched.current_task == task {
            sched.current_task = ptr::null_mut();
        }
        0
    })
}

fn select_next_task(sched: &mut SchedulerInner) -> *mut Task {
    let cpu_id = slopos_lib::get_current_cpu();
    let mut next = per_cpu::with_cpu_scheduler(cpu_id, |local| local.dequeue_highest_priority())
        .unwrap_or(ptr::null_mut());

    if next.is_null() {
        next = sched.dequeue_highest_priority();
    }

    if next.is_null() && !sched.idle_task.is_null() && !task_is_terminated(sched.idle_task) {
        next = sched.idle_task;
    }
    next
}

struct SwitchInfo {
    new_task: *mut Task,
    old_ctx_ptr: *mut TaskContext,
    is_user_mode: bool,
    rsp0: u64,
}

fn prepare_switch(sched: &mut SchedulerInner, new_task: *mut Task) -> Option<SwitchInfo> {
    if new_task.is_null() {
        return None;
    }

    let old_task = sched.current_task;
    if old_task == new_task {
        task_set_current(new_task);
        reset_task_quantum(sched, new_task);
        return None;
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(old_task, new_task, timestamp);

    sched.current_task = new_task;
    task_set_current(new_task);
    reset_task_quantum(sched, new_task);
    sched.total_switches += 1;

    let mut old_ctx_ptr: *mut TaskContext = ptr::null_mut();
    unsafe {
        if !old_task.is_null() && (*old_task).context_from_user == 0 {
            old_ctx_ptr = &raw mut (*old_task).context;
        } else if !old_task.is_null() {
            (*old_task).context_from_user = 0;
        }
    }

    unsafe {
        if (*new_task).process_id != INVALID_TASK_ID {
            let page_dir = process_vm_get_page_dir((*new_task).process_id);
            if !page_dir.is_null() && !(*page_dir).pml4_phys.is_null() {
                (*new_task).context.cr3 = (*page_dir).pml4_phys.as_u64();
                paging_set_current_directory(page_dir);
            }
        } else {
            paging_set_current_directory(paging_get_kernel_directory());
        }
    }

    let is_user_mode = unsafe { (*new_task).flags & TASK_FLAG_USER_MODE != 0 };
    let rsp0 = if is_user_mode {
        unsafe {
            if (*new_task).kernel_stack_top != 0 {
                (*new_task).kernel_stack_top
            } else {
                kernel_stack_top() as u64
            }
        }
    } else {
        kernel_stack_top() as u64
    };

    Some(SwitchInfo {
        new_task,
        old_ctx_ptr,
        is_user_mode,
        rsp0,
    })
}

fn do_context_switch(info: SwitchInfo, _preempt_guard: PreemptGuard) {
    let _balance = wl_currency::check_balance();

    platform::gdt_set_kernel_rsp0(info.rsp0);

    unsafe {
        if info.is_user_mode {
            context_switch_user(info.old_ctx_ptr, &(*info.new_task).context);
        } else if !info.old_ctx_ptr.is_null() {
            context_switch(info.old_ctx_ptr, &(*info.new_task).context);
        } else {
            context_switch(ptr::null_mut(), &(*info.new_task).context);
        }
    }
}

pub fn schedule() {
    // Acquire preempt guard FIRST - this prevents concurrent context switches
    let preempt_guard = PreemptGuard::new();

    enum ScheduleResult {
        Disabled,
        NoTask,
        IdleTerminated {
            current_ctx: *mut TaskContext,
            return_ctx: *const TaskContext,
        },
        Switch(SwitchInfo),
    }

    let result = with_scheduler(|sched| {
        if sched.enabled == 0 {
            return ScheduleResult::Disabled;
        }
        sched.schedule_calls = sched.schedule_calls.saturating_add(1);

        let current = sched.current_task;
        let cpu_id = slopos_lib::get_current_cpu();
        let is_idle = per_cpu::with_cpu_scheduler(cpu_id, |local| local.idle_task() == current)
            .unwrap_or(false)
            || current == sched.idle_task;

        if !current.is_null() && !is_idle {
            if task_is_running(current) {
                if task_set_state(unsafe { (*current).task_id }, TASK_STATE_READY) != 0 {
                    klog_info!("schedule: failed to mark task ready");
                } else if sched.enqueue_task(current) != 0 {
                    klog_info!("schedule: ready queue full when re-queuing task");
                    task_set_state(unsafe { (*current).task_id }, TASK_STATE_RUNNING);
                    reset_task_quantum(sched, current);
                    return ScheduleResult::NoTask;
                } else {
                    reset_task_quantum(sched, current);
                }
            } else if !task_is_blocked(current) && !task_is_terminated(current) {
                unsafe {
                    klog_info!("schedule: skipping requeue for task {}", (*current).task_id);
                }
            }
        }

        let next_task = select_next_task(sched);
        if next_task.is_null() {
            if !sched.idle_task.is_null() && task_is_terminated(sched.idle_task) {
                sched.enabled = 0;
                if !sched.current_task.is_null() {
                    let current_ctx = unsafe { &raw mut (*sched.current_task).context };
                    let return_ctx = &raw const sched.return_context;
                    return ScheduleResult::IdleTerminated {
                        current_ctx,
                        return_ctx,
                    };
                }
            }
            return ScheduleResult::NoTask;
        }

        match prepare_switch(sched, next_task) {
            Some(info) => ScheduleResult::Switch(info),
            None => ScheduleResult::NoTask,
        }
    });

    match result {
        ScheduleResult::Disabled | ScheduleResult::NoTask => {
            drop(preempt_guard);
        }
        ScheduleResult::IdleTerminated {
            current_ctx,
            return_ctx,
        } => unsafe {
            context_switch(current_ctx, return_ctx);
            drop(preempt_guard);
        },
        ScheduleResult::Switch(info) => {
            do_context_switch(info, preempt_guard);
        }
    }
}

pub fn r#yield() {
    with_scheduler(|sched| {
        sched.total_yields += 1;
        if !sched.current_task.is_null() {
            task_record_yield(sched.current_task);
        }
    });
    schedule();
}

pub fn yield_() {
    r#yield();
}

pub fn block_current_task() {
    let current = with_scheduler(|sched| sched.current_task);
    if current.is_null() {
        return;
    }
    if task_set_state(unsafe { (*current).task_id }, TASK_STATE_BLOCKED) != 0 {
        klog_info!("block_current_task: invalid state transition");
    }
    unschedule_task(current);
    schedule();
}

pub fn task_wait_for(task_id: u32) -> c_int {
    let current = with_scheduler(|sched| sched.current_task);
    if current.is_null() {
        return -1;
    }
    if task_id == INVALID_TASK_ID || unsafe { (*current).task_id } == task_id {
        return -1;
    }

    let mut target: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut target) != 0 || target.is_null() {
        unsafe { (*current).waiting_on_task_id = INVALID_TASK_ID };
        return 0;
    }
    unsafe { (*current).waiting_on_task_id = task_id };
    block_current_task();
    unsafe { (*current).waiting_on_task_id = INVALID_TASK_ID };
    0
}

pub fn unblock_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if task_set_state(unsafe { (*task).task_id }, TASK_STATE_READY) != 0 {
        unsafe {
            klog_info!(
                "unblock_task: invalid state transition for task {}",
                (*task).task_id
            );
        }
        return -1;
    }

    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

    schedule_task(task)
}

pub fn scheduler_task_exit_impl() -> ! {
    let current = with_scheduler(|sched| sched.current_task);
    if current.is_null() {
        klog_info!("scheduler_task_exit: No current task");
        schedule();
        loop {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(current, ptr::null_mut(), timestamp);

    if crate::task::task_terminate(u32::MAX) != 0 {
        klog_info!("scheduler_task_exit: Failed to terminate current task");
    }

    with_scheduler(|sched| {
        sched.current_task = ptr::null_mut();
    });
    task_set_current(ptr::null_mut());
    schedule();

    klog_info!("scheduler_task_exit: Schedule returned unexpectedly");
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

fn idle_task_function(_: *mut c_void) {
    loop {
        let cb = IDLE_WAKEUP_CB.get().and_then(|m| *m.lock());
        if let Some(callback) = cb {
            if callback() != 0 {
                r#yield();
                continue;
            }
        }
        let should_yield = with_scheduler(|sched| {
            sched.idle_time = sched.idle_time.saturating_add(1);
            sched.idle_time % 1000 == 0
        });
        if should_yield {
            r#yield();
        }
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

pub fn scheduler_register_idle_wakeup_callback(callback: Option<fn() -> c_int>) {
    IDLE_WAKEUP_CB.call_once(|| IrqMutex::new(None));
    if let Some(mutex) = IDLE_WAKEUP_CB.get() {
        *mutex.lock() = callback;
    }
}

fn deferred_reschedule_callback() {
    if !PreemptGuard::is_active() {
        let should_schedule =
            try_with_scheduler(|sched| sched.enabled != 0 && sched.preemption_enabled != 0);
        if should_schedule == Some(true) {
            schedule();
        }
    }
}

pub fn init_scheduler() -> c_int {
    SCHEDULER.call_once(|| IrqMutex::new(SchedulerInner::new()));
    with_scheduler(|sched| {
        sched.init_queues();
        sched.current_task = ptr::null_mut();
        sched.idle_task = ptr::null_mut();
        sched.policy = SCHED_POLICY_COOPERATIVE;
        sched.enabled = 0;
        sched.time_slice = SCHED_DEFAULT_TIME_SLICE as u16;
        sched.total_switches = 0;
        sched.total_yields = 0;
        sched.idle_time = 0;
        sched.schedule_calls = 0;
        sched.total_ticks = 0;
        sched.total_preemptions = 0;
        sched.preemption_enabled = SCHEDULER_PREEMPTION_DEFAULT;
    });
    user_copy::register_current_task_provider(current_task_process_id);

    per_cpu::init_all_percpu_schedulers();

    unsafe {
        slopos_lib::preempt::register_reschedule_callback(deferred_reschedule_callback);
    }

    slopos_lib::panic_recovery::register_panic_cleanup(sched_panic_cleanup);

    0
}

fn sched_panic_cleanup() {
    unsafe {
        scheduler_force_unlock();
        crate::task::task_manager_force_unlock();
    }
}

pub fn create_idle_task() -> c_int {
    create_idle_task_for_cpu(0)
}

pub fn create_idle_task_for_cpu(cpu_id: usize) -> c_int {
    let idle_task_id = unsafe {
        crate::task::task_create(
            b"idle\0".as_ptr() as *const i8,
            core::mem::transmute(idle_task_function as *const ()),
            ptr::null_mut(),
            TASK_PRIORITY_IDLE,
            TASK_FLAG_KERNEL_MODE,
        )
    };
    if idle_task_id == INVALID_TASK_ID {
        return -1;
    }
    let mut idle_task: *mut Task = ptr::null_mut();
    if task_get_info(idle_task_id, &mut idle_task) != 0 {
        return -1;
    }

    unsafe {
        (*idle_task).cpu_affinity = 1 << cpu_id;
        (*idle_task).last_cpu = cpu_id as u8;
    }

    if cpu_id == 0 {
        with_scheduler(|sched| {
            sched.idle_task = idle_task;
        });
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_idle_task(idle_task);
    });

    0
}

pub fn start_scheduler() -> c_int {
    let (already_enabled, has_ready_tasks) = with_scheduler(|sched| {
        if sched.enabled != 0 {
            return (true, false);
        }
        sched.enabled = 1;
        unsafe { crate::ffi_boundary::init_kernel_context(&raw mut sched.return_context) };
        let has_ready = sched.total_ready_count() > 0;
        (false, has_ready)
    });

    if already_enabled {
        return -1;
    }

    scheduler_set_preemption_enabled(SCHEDULER_PREEMPTION_DEFAULT as c_int);

    if has_ready_tasks {
        schedule();
    }

    let (current_null, idle_task) =
        with_scheduler(|sched| (sched.current_task.is_null(), sched.idle_task));

    if current_null && !idle_task.is_null() {
        with_scheduler(|sched| {
            sched.current_task = sched.idle_task;
            task_set_current(sched.idle_task);
            reset_task_quantum(sched, sched.idle_task);
        });
        idle_task_function(ptr::null_mut());
    } else if current_null {
        return -1;
    }

    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

pub fn stop_scheduler() {
    with_scheduler(|sched| {
        sched.enabled = 0;
    });
}

pub fn scheduler_shutdown() {
    with_scheduler(|sched| {
        sched.enabled = 0;
        sched.init_queues();
        sched.current_task = ptr::null_mut();
        sched.idle_task = ptr::null_mut();
    });
}

pub fn get_scheduler_stats(
    context_switches: *mut u64,
    yields: *mut u64,
    ready_tasks: *mut u32,
    schedule_calls: *mut u32,
) {
    with_scheduler(|sched| {
        if !context_switches.is_null() {
            unsafe { *context_switches = sched.total_switches };
        }
        if !yields.is_null() {
            unsafe { *yields = sched.total_yields };
        }
        if !schedule_calls.is_null() {
            unsafe { *schedule_calls = sched.schedule_calls };
        }
    });

    if !ready_tasks.is_null() {
        let global_count = with_scheduler(|sched| sched.total_ready_count());
        let percpu_count = per_cpu::get_total_ready_tasks();
        unsafe { *ready_tasks = global_count + percpu_count };
    }
}

pub fn scheduler_is_enabled() -> c_int {
    try_with_scheduler(|sched| sched.enabled as c_int).unwrap_or(0)
}

pub fn scheduler_get_current_task() -> *mut Task {
    let cpu_id = slopos_lib::get_current_cpu();
    let percpu_current = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.current_task())
        .unwrap_or(ptr::null_mut());

    if !percpu_current.is_null() {
        return percpu_current;
    }

    try_with_scheduler(|sched| sched.current_task).unwrap_or(ptr::null_mut())
}

pub fn scheduler_set_preemption_enabled(enabled: c_int) {
    let preemption_enabled = with_scheduler(|sched| {
        sched.preemption_enabled = if enabled != 0 { 1 } else { 0 };
        if sched.preemption_enabled == 0 {
            PreemptGuard::clear_reschedule_pending();
        }
        sched.preemption_enabled
    });
    if preemption_enabled != 0 {
        platform::timer_enable_irq();
    } else {
        platform::timer_disable_irq();
    }
}

pub fn scheduler_is_preemption_enabled() -> c_int {
    try_with_scheduler(|sched| sched.preemption_enabled as c_int).unwrap_or(0)
}

pub fn scheduler_timer_tick() {
    // If preemption is disabled via PreemptGuard, just mark pending
    if PreemptGuard::is_active() {
        PreemptGuard::set_reschedule_pending();
        return;
    }

    try_with_scheduler(|sched| {
        sched.total_ticks = sched.total_ticks.saturating_add(1);
        if sched.enabled == 0 || sched.preemption_enabled == 0 {
            return;
        }

        let current = sched.current_task;
        if current.is_null() {
            return;
        }
        if current == sched.idle_task {
            if sched.total_ready_count() > 0 {
                PreemptGuard::set_reschedule_pending();
            }
            return;
        }
        if unsafe { (*current).flags } & TASK_FLAG_NO_PREEMPT != 0 {
            return;
        }
        unsafe {
            if (*current).time_slice_remaining > 0 {
                (*current).time_slice_remaining -= 1;
            }
            if (*current).time_slice_remaining > 0 {
                return;
            }
        }
        if sched.total_ready_count() == 0 {
            reset_task_quantum(sched, current);
            return;
        }
        sched.total_preemptions = sched.total_preemptions.saturating_add(1);
        PreemptGuard::set_reschedule_pending();
    });
}

pub fn scheduler_request_reschedule_from_interrupt() {
    let should_set =
        try_with_scheduler(|sched| sched.enabled != 0 && sched.preemption_enabled != 0);
    if should_set == Some(true) && !PreemptGuard::is_active() {
        PreemptGuard::set_reschedule_pending();
    }
}

pub fn scheduler_handle_post_irq() {
    if PreemptGuard::is_active() {
        return;
    }

    if !PreemptGuard::is_reschedule_pending() {
        return;
    }

    let should_schedule =
        try_with_scheduler(|sched| sched.enabled != 0 && sched.preemption_enabled != 0);

    if should_schedule == Some(true) {
        PreemptGuard::clear_reschedule_pending();
        schedule();
    }
}

pub fn boot_step_task_manager_init() -> c_int {
    crate::task::init_task_manager()
}

pub fn boot_step_scheduler_init() -> c_int {
    init_scheduler()
}

pub fn boot_step_idle_task() -> c_int {
    create_idle_task()
}

pub fn init_scheduler_for_ap(cpu_id: usize) {
    per_cpu::init_percpu_scheduler(cpu_id);

    // Create idle task for this AP. At this point BSP has already initialized
    // the task manager and heap, so it's safe to allocate.
    if cpu_id != 0 {
        let idle = per_cpu::create_ap_idle_task(cpu_id);
        if idle.is_null() {
            klog_info!(
                "SCHED: Warning - failed to create idle task for CPU {}",
                cpu_id
            );
        }
    }
}

pub fn scheduler_run_ap(cpu_id: usize) -> ! {
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.enable();
    });

    slopos_lib::mark_cpu_online(cpu_id);
    klog_info!("SCHED: CPU {} scheduler online", cpu_id);

    let idle_task =
        per_cpu::with_cpu_scheduler(cpu_id, |s| s.idle_task()).unwrap_or(ptr::null_mut());

    if idle_task.is_null() {
        klog_info!("SCHED: CPU {} has no idle task, halting", cpu_id);
        loop {
            unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
        }
    }

    unsafe {
        let return_ctx = per_cpu::get_ap_return_context(cpu_id);
        if !return_ctx.is_null() {
            crate::ffi_boundary::init_kernel_context(return_ctx);
        }
    }

    ap_scheduler_loop(cpu_id, idle_task);
}

fn ap_scheduler_loop(cpu_id: usize, idle_task: *mut Task) -> ! {
    use super::work_steal::try_work_steal;

    loop {
        if per_cpu::are_aps_paused() {
            unsafe {
                core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
            }
            continue;
        }

        let next_task =
            per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.dequeue_highest_priority())
                .unwrap_or(ptr::null_mut());

        if !next_task.is_null() {
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                sched.set_executing_task(true);
            });

            if per_cpu::are_aps_paused() {
                per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                    sched.enqueue_local(next_task);
                    sched.set_executing_task(false);
                });
                core::hint::spin_loop();
                continue;
            }
            if task_is_terminated(next_task) || !task_is_ready(next_task) {
                per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                    sched.set_executing_task(false);
                });
                continue;
            }
            ap_execute_task(cpu_id, idle_task, next_task);
            continue;
        }

        if !per_cpu::are_aps_paused() && try_work_steal() {
            continue;
        }

        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.increment_idle_time();
        });

        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
    }
}

fn ap_execute_task(cpu_id: usize, idle_task: *mut Task, next_task: *mut Task) {
    if task_is_terminated(next_task) || !task_is_ready(next_task) {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        return;
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(idle_task, next_task, timestamp);

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(next_task);
        sched.increment_switches();
    });

    task_set_current(next_task);

    unsafe {
        let is_user_mode = (*next_task).flags & TASK_FLAG_USER_MODE != 0;
        let kernel_rsp = if is_user_mode && (*next_task).kernel_stack_top != 0 {
            (*next_task).kernel_stack_top
        } else {
            kernel_stack_top() as u64
        };

        platform::gdt_set_kernel_rsp0(kernel_rsp);

        if (*next_task).process_id != INVALID_TASK_ID {
            let page_dir = process_vm_get_page_dir((*next_task).process_id);
            if !page_dir.is_null() && !(*page_dir).pml4_phys.is_null() {
                (*next_task).context.cr3 = (*page_dir).pml4_phys.as_u64();
                paging_set_current_directory(page_dir);
            }
        } else {
            paging_set_current_directory(paging_get_kernel_directory());
        }

        let idle_ctx = &raw mut (*idle_task).context;

        if is_user_mode {
            context_switch_user(idle_ctx, &(*next_task).context);
        } else {
            context_switch(idle_ctx, &(*next_task).context);
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(next_task, idle_task, timestamp);

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(idle_task);
    });

    task_set_current(idle_task);

    unsafe {
        if !task_is_terminated(next_task) && task_is_running(next_task) {
            if task_set_state((*next_task).task_id, TASK_STATE_READY) == 0 {
                per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                    sched.enqueue_local(next_task);
                });
            }
        }
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(false);
    });
}

pub fn get_percpu_scheduler_stats(
    cpu_id: usize,
    switches: *mut u64,
    preemptions: *mut u64,
    ready_tasks: *mut u32,
) {
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        if !switches.is_null() {
            unsafe { *switches = sched.total_switches.load(Ordering::Relaxed) };
        }
        if !preemptions.is_null() {
            unsafe { *preemptions = sched.total_preemptions.load(Ordering::Relaxed) };
        }
        if !ready_tasks.is_null() {
            unsafe { *ready_tasks = sched.total_ready_count() };
        }
    });
}

pub fn get_total_ready_tasks_all_cpus() -> u32 {
    per_cpu::get_total_ready_tasks()
}

#[allow(dead_code)]
pub fn send_reschedule_ipi(target_cpu: usize) {
    use slopos_abi::arch::x86_64::idt::RESCHEDULE_IPI_VECTOR;

    let current_cpu = slopos_lib::get_current_cpu();
    if target_cpu == current_cpu {
        return;
    }

    if let Some(apic_id) = slopos_lib::apic_id_from_cpu_index(target_cpu) {
        slopos_lib::send_ipi_to_cpu(apic_id, RESCHEDULE_IPI_VECTOR);
    }
}

pub unsafe fn scheduler_force_unlock() {
    if let Some(mutex) = SCHEDULER.get() {
        unsafe { mutex.force_unlock() };
    }
}
