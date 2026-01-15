use core::ffi::{c_int, c_void};
use core::ptr;

use slopos_lib::IrqMutex;
use spin::Once;

use slopos_lib::kdiag_timestamp;
use slopos_lib::klog_info;

use crate::platform;
use crate::wl_currency;

use super::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE,
    TASK_PRIORITY_IDLE, TASK_STATE_BLOCKED, TASK_STATE_READY, TASK_STATE_RUNNING, Task,
    TaskContext, task_get_info, task_get_state, task_is_blocked, task_is_ready, task_is_running,
    task_is_terminated, task_record_context_switch, task_record_yield, task_set_current,
    task_set_state,
};

const SCHED_DEFAULT_TIME_SLICE: u32 = 10;
const SCHED_POLICY_COOPERATIVE: u8 = 2;
const SCHEDULER_PREEMPTION_DEFAULT: u8 = 1;

/// Number of priority levels (HIGH=0, NORMAL=1, LOW=2, IDLE=3)
const NUM_PRIORITY_LEVELS: usize = 4;

#[derive(Default)]
struct ReadyQueue {
    head: *mut Task,
    tail: *mut Task,
    count: u32,
}

// SAFETY: ReadyQueue contains raw pointers to Task which live in static storage.
// Access is serialized through the SchedulerInner mutex.
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
    reschedule_pending: u8,
    in_schedule: u8,
}

// SAFETY: SchedulerInner contains raw pointers to Task which live in static storage.
// All access is serialized through the mutex.
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
            reschedule_pending: 0,
            in_schedule: 0,
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
        if sched.enqueue_task(task) != 0 {
            klog_info!("schedule_task: ready queue full, request rejected");
            return -1;
        }
        0
    })
}

pub fn unschedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    with_scheduler(|sched| {
        sched.remove_task(task);
        if sched.current_task == task {
            sched.current_task = ptr::null_mut();
        }
        0
    })
}

fn select_next_task(sched: &mut SchedulerInner) -> *mut Task {
    let mut next = sched.dequeue_highest_priority();
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

fn do_context_switch(info: SwitchInfo) {
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
        sched.in_schedule = sched.in_schedule.saturating_add(1);
        sched.schedule_calls = sched.schedule_calls.saturating_add(1);

        let current = sched.current_task;
        if !current.is_null() && current != sched.idle_task {
            if task_is_running(current) {
                if task_set_state(unsafe { (*current).task_id }, TASK_STATE_READY) != 0 {
                    klog_info!("schedule: failed to mark task ready");
                } else if sched.enqueue_task(current) != 0 {
                    klog_info!("schedule: ready queue full when re-queuing task");
                    task_set_state(unsafe { (*current).task_id }, TASK_STATE_RUNNING);
                    reset_task_quantum(sched, current);
                    sched.in_schedule = sched.in_schedule.saturating_sub(1);
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
                    sched.in_schedule = sched.in_schedule.saturating_sub(1);
                    let current_ctx = unsafe { &raw mut (*sched.current_task).context };
                    let return_ctx = &raw const sched.return_context;
                    return ScheduleResult::IdleTerminated {
                        current_ctx,
                        return_ctx,
                    };
                }
            }
            sched.in_schedule = sched.in_schedule.saturating_sub(1);
            return ScheduleResult::NoTask;
        }

        sched.in_schedule = sched.in_schedule.saturating_sub(1);
        match prepare_switch(sched, next_task) {
            Some(info) => ScheduleResult::Switch(info),
            None => ScheduleResult::NoTask,
        }
    });

    match result {
        ScheduleResult::Disabled | ScheduleResult::NoTask => {}
        ScheduleResult::IdleTerminated {
            current_ctx,
            return_ctx,
        } => unsafe {
            context_switch(current_ctx, return_ctx);
        },
        ScheduleResult::Switch(info) => {
            do_context_switch(info);
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
    }
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
        sched.reschedule_pending = 0;
        sched.in_schedule = 0;
    });
    user_copy::register_current_task_provider(current_task_process_id);
    0
}
pub fn create_idle_task() -> c_int {
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
    with_scheduler(|sched| {
        sched.idle_task = idle_task;
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
        if !ready_tasks.is_null() {
            unsafe { *ready_tasks = sched.total_ready_count() };
        }
        if !schedule_calls.is_null() {
            unsafe { *schedule_calls = sched.schedule_calls };
        }
    });
}

pub fn scheduler_is_enabled() -> c_int {
    try_with_scheduler(|sched| sched.enabled as c_int).unwrap_or(0)
}

pub fn scheduler_get_current_task() -> *mut Task {
    try_with_scheduler(|sched| sched.current_task).unwrap_or(ptr::null_mut())
}

pub fn scheduler_set_preemption_enabled(enabled: c_int) {
    let preemption_enabled = with_scheduler(|sched| {
        sched.preemption_enabled = if enabled != 0 { 1 } else { 0 };
        if sched.preemption_enabled == 0 {
            sched.reschedule_pending = 0;
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
    try_with_scheduler(|sched| {
        sched.total_ticks = sched.total_ticks.saturating_add(1);
        if sched.enabled == 0 || sched.preemption_enabled == 0 {
            return;
        }

        let current = sched.current_task;
        if current.is_null() {
            return;
        }
        if sched.in_schedule != 0 {
            return;
        }
        if current == sched.idle_task {
            if sched.total_ready_count() > 0 {
                sched.reschedule_pending = 1;
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
        if sched.reschedule_pending == 0 {
            sched.total_preemptions = sched.total_preemptions.saturating_add(1);
        }
        sched.reschedule_pending = 1;
    });
}

pub fn scheduler_request_reschedule_from_interrupt() {
    try_with_scheduler(|sched| {
        if sched.enabled == 0 || sched.preemption_enabled == 0 {
            return;
        }
        if sched.in_schedule == 0 {
            sched.reschedule_pending = 1;
        }
    });
}
pub fn scheduler_handle_post_irq() {
    let should_schedule = try_with_scheduler(|sched| {
        if sched.reschedule_pending == 0 {
            return false;
        }
        if sched.enabled == 0 || sched.preemption_enabled == 0 {
            sched.reschedule_pending = 0;
            return false;
        }
        if sched.in_schedule != 0 {
            return false;
        }
        sched.reschedule_pending = 0;
        true
    });
    if should_schedule == Some(true) {
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
