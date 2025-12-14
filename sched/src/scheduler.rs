#![allow(dead_code)]

use core::ffi::{c_int, c_void};
use core::ptr;

use slopos_drivers::wl_currency;
use slopos_lib::kdiag_timestamp;
use slopos_lib::{klog_debug, klog_info};

use crate::task::{
    task_get_info, task_get_state, task_is_blocked, task_is_ready, task_is_running, task_is_terminated,
    task_record_context_switch, task_record_yield, task_set_current, task_set_state, Task, TaskContext,
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE, TASK_PRIORITY_IDLE,
    TASK_STATE_BLOCKED, TASK_STATE_READY, TASK_STATE_RUNNING,
};

#[repr(C)]
pub struct Scheduler {
    ready_queue: ReadyQueue,
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

#[derive(Default)]
struct ReadyQueue {
    head: *mut Task,
    tail: *mut Task,
    count: u32,
}

static mut SCHEDULER: Scheduler = Scheduler {
    ready_queue: ReadyQueue {
        head: ptr::null_mut(),
        tail: ptr::null_mut(),
        count: 0,
    },
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
};

static mut IDLE_WAKEUP_CB: Option<extern "C" fn() -> c_int> = None;

const SCHED_DEFAULT_TIME_SLICE: u32 = 10;
const SCHED_POLICY_COOPERATIVE: u8 = 2;
const SCHEDULER_PREEMPTION_DEFAULT: u8 = 1;

use slopos_mm::paging::{paging_set_current_directory, paging_get_kernel_directory};
use slopos_mm::process_vm::process_vm_get_page_dir;
use slopos_drivers::pit::{pit_enable_irq, pit_disable_irq};
use slopos_drivers::scheduler_callbacks::call_gdt_set_kernel_rsp0;

unsafe extern "C" {
    fn context_switch(old_context: *mut TaskContext, new_context: *const TaskContext);
    fn context_switch_user(old_context: *mut TaskContext, new_context: *const TaskContext);
    fn simple_context_switch(old_context: *mut TaskContext, new_context: *const TaskContext);
    fn init_kernel_context(context: *mut TaskContext);
    fn task_entry_wrapper();

    static kernel_stack_top: u8;
}

#[repr(C)]
pub struct ProcessPageDir {
    pub pml4: *mut PageTable,
    pub pml4_phys: u64,
    pub ref_count: u32,
    pub process_id: u32,
    pub next: *mut ProcessPageDir,
}

#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [u64; 512],
}

fn scheduler_mut() -> *mut Scheduler {
    &raw mut SCHEDULER
}

fn ready_queue_init(queue: &mut ReadyQueue) {
    queue.head = ptr::null_mut();
    queue.tail = ptr::null_mut();
    queue.count = 0;
}

fn ready_queue_empty(queue: &ReadyQueue) -> bool {
    queue.count == 0
}

fn ready_queue_contains(queue: &ReadyQueue, task: *mut Task) -> bool {
    let mut cursor = queue.head;
    while !cursor.is_null() {
        if cursor == task {
            return true;
        }
        unsafe {
            cursor = (*cursor).next_ready;
        }
    }
    false
}

fn ready_queue_enqueue(queue: &mut ReadyQueue, task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if ready_queue_contains(queue, task) {
        return 0;
    }
    unsafe { (*task).next_ready = ptr::null_mut() };
    if queue.head.is_null() {
        queue.head = task;
        queue.tail = task;
    } else {
        unsafe { (*queue.tail).next_ready = task };
        queue.tail = task;
    }
    queue.count += 1;
    0
}

fn ready_queue_dequeue(queue: &mut ReadyQueue) -> *mut Task {
    if ready_queue_empty(queue) {
        return ptr::null_mut();
    }
    let task = queue.head;
    unsafe {
        queue.head = (*task).next_ready;
        if queue.head.is_null() {
            queue.tail = ptr::null_mut();
        }
        (*task).next_ready = ptr::null_mut();
    }
    queue.count -= 1;
    task
}

fn ready_queue_remove(queue: &mut ReadyQueue, task: *mut Task) -> c_int {
    if task.is_null() || ready_queue_empty(queue) {
        return -1;
    }
    let mut prev: *mut Task = ptr::null_mut();
    let mut cursor = queue.head;
    while !cursor.is_null() {
        if cursor == task {
            if !prev.is_null() {
                unsafe { (*prev).next_ready = (*cursor).next_ready };
            } else {
                queue.head = unsafe { (*cursor).next_ready };
            }
            if queue.tail == cursor {
                queue.tail = prev;
            }
            unsafe { (*cursor).next_ready = ptr::null_mut() };
            queue.count -= 1;
            return 0;
        }
        prev = cursor;
        unsafe {
            cursor = (*cursor).next_ready;
        }
    }
    -1
}

fn scheduler_get_default_time_slice() -> u64 {
    unsafe {
        let sched = &mut *scheduler_mut();
        if sched.time_slice != 0 {
            sched.time_slice as u64
        } else {
            SCHED_DEFAULT_TIME_SLICE as u64
        }
    }
}

fn scheduler_reset_task_quantum(task: *mut Task) {
    if task.is_null() {
        return;
    }
    let slice = unsafe {
        if (*task).time_slice != 0 {
            (*task).time_slice
        } else {
            scheduler_get_default_time_slice()
        }
    };
    unsafe {
        (*task).time_slice = slice;
        (*task).time_slice_remaining = slice;
    }
}

#[unsafe(no_mangle)]
pub fn schedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    let sched = unsafe { &mut *scheduler_mut() };
    if !task_is_ready(task) {
        unsafe {
            klog_info!("schedule_task: task {} not ready (state {})", (*task).task_id, task_get_state(task) as u32);
        }
        return -1;
    }
    if unsafe { (*task).time_slice_remaining } == 0 {
        scheduler_reset_task_quantum(task);
    }
    if ready_queue_enqueue(&mut sched.ready_queue, task) != 0 {
        klog_info!("schedule_task: ready queue full, request rejected");
        wl_currency::award_loss();
        return -1;
    }
    unsafe {
        klog_debug!("schedule_task: enqueued task {} (flags=0x{:x}) ready_count={}", (*task).task_id, (*task).flags as u32, sched.ready_queue.count);
    }
    0
}

#[unsafe(no_mangle)]
pub fn unschedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    let sched = unsafe { &mut *scheduler_mut() };
    ready_queue_remove(&mut sched.ready_queue, task);
    if sched.current_task == task {
        sched.current_task = ptr::null_mut();
    }
    0
}

fn select_next_task() -> *mut Task {
    let sched = unsafe { &mut *scheduler_mut() };
    let mut next = ready_queue_dequeue(&mut sched.ready_queue);
    if next.is_null() && !sched.idle_task.is_null() && !task_is_terminated(sched.idle_task) {
        next = sched.idle_task;
    }
    next
}

fn switch_to_task(new_task: *mut Task) {
    if new_task.is_null() {
        return;
    }
    let sched = unsafe { &mut *scheduler_mut() };
    let old_task = sched.current_task;
    if old_task == new_task {
        return;
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(old_task, new_task, timestamp);

    sched.current_task = new_task;
    task_set_current(new_task);
    scheduler_reset_task_quantum(new_task);
    sched.total_switches += 1;

    unsafe {
        klog_debug!("switch_to_task: now running task {} (flags=0x{:x} pid={})", (*new_task).task_id, (*new_task).flags as u32, (*new_task).process_id);
    }

    let mut old_ctx_ptr: *mut TaskContext = ptr::null_mut();
    unsafe {
        if !old_task.is_null() && (*old_task).context_from_user == 0 {
            old_ctx_ptr = &mut (*old_task).context;
        } else if !old_task.is_null() {
            (*old_task).context_from_user = 0;
        }
    }

    unsafe {
        if (*new_task).process_id != INVALID_TASK_ID {
            let page_dir = process_vm_get_page_dir((*new_task).process_id);
            if !page_dir.is_null() && (*page_dir).pml4_phys != 0 {
                (*new_task).context.cr3 = (*page_dir).pml4_phys;
                paging_set_current_directory(page_dir);
            }
        } else {
            paging_set_current_directory(paging_get_kernel_directory());
        }
    }

    wl_currency::check_balance();

    unsafe {
        if (*new_task).flags & TASK_FLAG_USER_MODE != 0 {
            let rsp0 = if (*new_task).kernel_stack_top != 0 {
                (*new_task).kernel_stack_top
            } else {
                &kernel_stack_top as *const u8 as u64
            };
            call_gdt_set_kernel_rsp0(rsp0);
            context_switch_user(old_ctx_ptr, &(*new_task).context);
        } else {
            call_gdt_set_kernel_rsp0(&kernel_stack_top as *const u8 as u64);
            if !old_ctx_ptr.is_null() {
                context_switch(old_ctx_ptr, &(*new_task).context);
            } else {
                context_switch(ptr::null_mut(), &(*new_task).context);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schedule() {
    let sched = unsafe { &mut *scheduler_mut() };
    if sched.enabled == 0 {
        return;
    }
    sched.in_schedule = sched.in_schedule.saturating_add(1);
    sched.schedule_calls = sched.schedule_calls.saturating_add(1);

    let current = sched.current_task;
    if !current.is_null() && current != sched.idle_task {
        if task_is_running(current) {
            if task_set_state(unsafe { (*current).task_id }, TASK_STATE_READY) != 0 {
                klog_info!("schedule: failed to mark task ready");
            } else if ready_queue_enqueue(&mut sched.ready_queue, current) != 0 {
                klog_info!("schedule: ready queue full when re-queuing task");
                task_set_state(unsafe { (*current).task_id }, TASK_STATE_RUNNING);
                scheduler_reset_task_quantum(current);
                sched.in_schedule = sched.in_schedule.saturating_sub(1);
                return;
            } else {
                scheduler_reset_task_quantum(current);
            }
        } else if !task_is_blocked(current) && !task_is_terminated(current) {
            unsafe {
                klog_info!("schedule: skipping requeue for task {}", (*current).task_id);
            }
        }
    }

    let next_task = select_next_task();
    if next_task.is_null() {
        if !sched.idle_task.is_null() && task_is_terminated(sched.idle_task) {
            sched.enabled = 0;
            if !sched.current_task.is_null() {
                sched.in_schedule = sched.in_schedule.saturating_sub(1);
                unsafe {
                    context_switch(&mut (*sched.current_task).context, &sched.return_context);
                }
                return;
            }
        }
        sched.in_schedule = sched.in_schedule.saturating_sub(1);
        return;
    }

    sched.in_schedule = sched.in_schedule.saturating_sub(1);
    switch_to_task(next_task);
}

#[unsafe(no_mangle)]
pub fn r#yield() {
    let sched = unsafe { &mut *scheduler_mut() };
    sched.total_yields += 1;
    if !sched.current_task.is_null() {
        task_record_yield(sched.current_task);
    }
    schedule();
}

// C-ABI shim expected by syscall and TTY code.
#[unsafe(no_mangle)]
pub extern "C" fn yield_() {
    r#yield();
}

#[unsafe(no_mangle)]
pub fn block_current_task() {
    let sched = unsafe { &mut *scheduler_mut() };
    let current = sched.current_task;
    if current.is_null() {
        return;
    }
    if task_set_state(unsafe { (*current).task_id }, TASK_STATE_BLOCKED) != 0 {
        klog_info!("block_current_task: invalid state transition");
    }
    unschedule_task(current);
    schedule();
}

#[unsafe(no_mangle)]
pub fn task_wait_for(task_id: u32) -> c_int {
    let sched = unsafe { &mut *scheduler_mut() };
    let current = sched.current_task;
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

#[unsafe(no_mangle)]
pub fn unblock_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if task_set_state(unsafe { (*task).task_id }, TASK_STATE_READY) != 0 {
        unsafe {
            klog_info!("unblock_task: invalid state transition for task {}", (*task).task_id);
        }
    }
    schedule_task(task)
}

#[unsafe(no_mangle)]
pub extern "C" fn scheduler_task_exit() -> ! {
    let sched = unsafe { &mut *scheduler_mut() };
    let current = sched.current_task;
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

    sched.current_task = ptr::null_mut();
    task_set_current(ptr::null_mut());
    schedule();

    klog_info!("scheduler_task_exit: Schedule returned unexpectedly");
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

extern "C" fn idle_task_function(_: *mut c_void) {
    let sched = unsafe { &mut *scheduler_mut() };
    loop {
        if let Some(cb) = unsafe { IDLE_WAKEUP_CB } {
            if cb() != 0 {
                r#yield();
                continue;
            }
        }
        sched.idle_time = sched.idle_time.saturating_add(1);
        if sched.idle_time % 1000 == 0 {
            r#yield();
        }
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn scheduler_register_idle_wakeup_callback(callback: Option<extern "C" fn() -> c_int>) {
    unsafe {
        IDLE_WAKEUP_CB = callback;
    }
}

#[unsafe(no_mangle)]
pub fn init_scheduler() -> c_int {
    let sched = unsafe { &mut *scheduler_mut() };
    ready_queue_init(&mut sched.ready_queue);
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
    0
}

#[unsafe(no_mangle)]
pub fn create_idle_task() -> c_int {
    let idle_task_id = crate::task::task_create(
        b"idle\0".as_ptr() as *const i8,
        idle_task_function,
        ptr::null_mut(),
        TASK_PRIORITY_IDLE,
        TASK_FLAG_KERNEL_MODE,
    );
    if idle_task_id == INVALID_TASK_ID {
        return -1;
    }
    let mut idle_task: *mut Task = ptr::null_mut();
    if task_get_info(idle_task_id, &mut idle_task) != 0 {
        return -1;
    }
    unsafe { (*scheduler_mut()).idle_task = idle_task };
    0
}

#[unsafe(no_mangle)]
pub fn start_scheduler() -> c_int {
    let sched = unsafe { &mut *scheduler_mut() };
    if sched.enabled != 0 {
        return -1;
    }
    sched.enabled = 1;
    unsafe { init_kernel_context(&mut sched.return_context) };
    scheduler_set_preemption_enabled(SCHEDULER_PREEMPTION_DEFAULT as c_int);

    klog_debug!("start_scheduler: ready_count={} idle_task={:p}", sched.ready_queue.count, sched.idle_task);

    if !ready_queue_empty(&sched.ready_queue) {
        schedule();
    }

    if sched.current_task.is_null() && !sched.idle_task.is_null() {
        sched.current_task = sched.idle_task;
        task_set_current(sched.idle_task);
        scheduler_reset_task_quantum(sched.idle_task);
        idle_task_function(ptr::null_mut());
    } else if sched.current_task.is_null() {
        return -1;
    }

    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

#[unsafe(no_mangle)]
pub fn stop_scheduler() {
    unsafe { (*scheduler_mut()).enabled = 0 };
}

#[unsafe(no_mangle)]
pub fn scheduler_shutdown() {
    let sched = unsafe { &mut *scheduler_mut() };
    sched.enabled = 0;
    ready_queue_init(&mut sched.ready_queue);
    sched.current_task = ptr::null_mut();
    sched.idle_task = ptr::null_mut();
}

#[unsafe(no_mangle)]
pub extern "C" fn get_scheduler_stats(
    context_switches: *mut u64,
    yields: *mut u64,
    ready_tasks: *mut u32,
    schedule_calls: *mut u32,
) {
    let sched = unsafe { &mut *scheduler_mut() };
    unsafe {
        if !context_switches.is_null() {
            *context_switches = sched.total_switches;
        }
        if !yields.is_null() {
            *yields = sched.total_yields;
        }
        if !ready_tasks.is_null() {
            *ready_tasks = sched.ready_queue.count;
        }
        if !schedule_calls.is_null() {
            *schedule_calls = sched.schedule_calls;
        }
    }
}

#[unsafe(no_mangle)]
pub fn scheduler_is_enabled() -> c_int {
    unsafe { (*scheduler_mut()).enabled as c_int }
}

#[unsafe(no_mangle)]
pub extern "C" fn scheduler_get_current_task() -> *mut Task {
    unsafe { (*scheduler_mut()).current_task }
}

#[unsafe(no_mangle)]
pub fn scheduler_set_preemption_enabled(enabled: c_int) {
    let sched = unsafe { &mut *scheduler_mut() };
    sched.preemption_enabled = if enabled != 0 { 1 } else { 0 };
    if sched.preemption_enabled != 0 {
        pit_enable_irq();
    } else {
        sched.reschedule_pending = 0;
        pit_disable_irq();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn scheduler_is_preemption_enabled() -> c_int {
    unsafe { (*scheduler_mut()).preemption_enabled as c_int }
}

#[unsafe(no_mangle)]
pub extern "C" fn scheduler_timer_tick() {
    let sched = unsafe { &mut *scheduler_mut() };
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
        if sched.ready_queue.count > 0 {
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
    if sched.ready_queue.count == 0 {
        scheduler_reset_task_quantum(current);
        return;
    }
    if sched.reschedule_pending == 0 {
        sched.total_preemptions = sched.total_preemptions.saturating_add(1);
    }
    sched.reschedule_pending = 1;
}

#[unsafe(no_mangle)]
pub extern "C" fn scheduler_request_reschedule_from_interrupt() {
    let sched = unsafe { &mut *scheduler_mut() };
    if sched.enabled == 0 || sched.preemption_enabled == 0 {
        return;
    }
    if sched.in_schedule == 0 {
        sched.reschedule_pending = 1;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn scheduler_handle_post_irq() {
    let sched = unsafe { &mut *scheduler_mut() };
    if sched.reschedule_pending == 0 {
        return;
    }
    if sched.enabled == 0 || sched.preemption_enabled == 0 {
        sched.reschedule_pending = 0;
        return;
    }
    if sched.in_schedule != 0 {
        return;
    }
    sched.reschedule_pending = 0;
    schedule();
}

#[unsafe(no_mangle)]
pub fn boot_step_task_manager_init() -> c_int {
    crate::task::init_task_manager()
}

#[unsafe(no_mangle)]
pub fn boot_step_scheduler_init() -> c_int {
    let rc = init_scheduler();
    if rc == 0 {
        // Register scheduler callbacks with drivers to break circular dependency
        unsafe {
            use slopos_drivers::scheduler_callbacks::SchedulerCallbacks;
            use core::ffi::c_void;
            use crate::task::task_terminate;
            // Cast the function pointer to use c_void pointer to avoid type mismatch, and convert extern "C" fn to fn
            let get_current_task_fn: fn() -> *mut c_void = core::mem::transmute(scheduler_get_current_task as *const ());
            slopos_drivers::scheduler_callbacks::register_callbacks(SchedulerCallbacks {
                timer_tick: Some(core::mem::transmute(scheduler_timer_tick as *const ())),
                handle_post_irq: Some(core::mem::transmute(scheduler_handle_post_irq as *const ())),
                request_reschedule_from_interrupt: Some(core::mem::transmute(scheduler_request_reschedule_from_interrupt as *const ())),
                get_current_task: Some(get_current_task_fn),
                yield_fn: Some(core::mem::transmute(yield_ as *const ())),
                schedule_fn: Some(core::mem::transmute(schedule as *const ())),
                task_terminate_fn: Some(core::mem::transmute(task_terminate as *const ())),
                scheduler_is_preemption_enabled_fn: Some(core::mem::transmute(scheduler_is_preemption_enabled as *const ())),
                get_task_stats_fn: Some(core::mem::transmute(crate::task::get_task_stats as *const ())),
                get_scheduler_stats_fn: Some(core::mem::transmute(get_scheduler_stats as *const ())),
            });
        }
        
        // Register scheduler callbacks for boot to break circular dependency
        unsafe {
            use slopos_drivers::scheduler_callbacks::SchedulerCallbacksForBoot;
            // Cast Task pointer to opaque Task type in drivers, and convert extern "C" fn to fn via pointer cast
            let get_current_task_boot_fn: fn() -> *mut slopos_drivers::scheduler_callbacks::Task = 
                core::mem::transmute(scheduler_get_current_task as *const ());
            slopos_drivers::scheduler_callbacks::register_scheduler_callbacks_for_boot(SchedulerCallbacksForBoot {
                request_reschedule_from_interrupt: Some(core::mem::transmute(scheduler_request_reschedule_from_interrupt as *const ())),
                get_current_task: Some(get_current_task_boot_fn),
                task_terminate: Some(core::mem::transmute(crate::task::task_terminate as *const ())),
            });
        }
    }
    rc
}

#[unsafe(no_mangle)]
pub fn boot_step_idle_task() -> c_int {
    create_idle_task()
}
