#![allow(dead_code)]

use core::ffi::{c_int, c_void};
use core::ptr;

use slopos_drivers::wl_currency;
use slopos_lib::kdiag_timestamp;
use slopos_lib::klog::{klog_printf, KlogLevel};

use crate::task::{
    task_get_info, task_get_state, task_is_blocked, task_is_ready, task_is_running, task_is_terminated,
    task_record_context_switch, task_record_yield, task_set_current, task_set_state, Task, TaskContext,
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE, TASK_PRIORITY_IDLE,
    TASK_STATE_BLOCKED, TASK_STATE_INVALID, TASK_STATE_READY, TASK_STATE_RUNNING, TASK_STATE_TERMINATED,
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

extern "C" {
    fn gdt_set_kernel_rsp0(rsp0: u64);
    fn paging_set_current_directory(page_dir: *mut ProcessPageDir);
    fn paging_get_kernel_directory() -> *mut ProcessPageDir;
    fn process_vm_get_page_dir(process_id: u32) -> *mut ProcessPageDir;
    fn pit_enable_irq();
    fn pit_disable_irq();

    static kernel_stack_top: u8;

    fn is_kernel_initialized() -> i32;
}

#[no_mangle]
pub extern "C" fn init_kernel_context(_context: *mut TaskContext) {}

#[no_mangle]
pub extern "C" fn task_entry_wrapper() {}

#[no_mangle]
pub extern "C" fn context_switch(old_context: *mut TaskContext, new_context: *const TaskContext) {
    unsafe {
        if !old_context.is_null() && !new_context.is_null() {
            *old_context = *new_context;
        }
    }
}

#[no_mangle]
pub extern "C" fn context_switch_user(
    old_context: *mut TaskContext,
    new_context: *const TaskContext,
) {
    context_switch(old_context, new_context);
}

#[no_mangle]
pub extern "C" fn simple_context_switch(
    old_context: *mut TaskContext,
    new_context: *const TaskContext,
) {
    context_switch(old_context, new_context);
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

fn scheduler_mut() -> &'static mut Scheduler {
    unsafe { &mut SCHEDULER }
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
    let sched = scheduler_mut();
    if sched.time_slice != 0 {
        sched.time_slice as u64
    } else {
        SCHED_DEFAULT_TIME_SLICE as u64
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

#[no_mangle]
pub extern "C" fn schedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    let sched = scheduler_mut();
    if !task_is_ready(task) {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"schedule_task: task %u not ready (state %u)\n\0".as_ptr() as *const i8,
                (*task).task_id,
                task_get_state(task) as u32,
            );
        }
        return -1;
    }
    if unsafe { (*task).time_slice_remaining } == 0 {
        scheduler_reset_task_quantum(task);
    }
    if ready_queue_enqueue(&mut sched.ready_queue, task) != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"schedule_task: ready queue full, request rejected\n\0".as_ptr() as *const i8,
            );
        }
        wl_currency::award_loss();
        return -1;
    }
    0
}

#[no_mangle]
pub extern "C" fn unschedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    let sched = scheduler_mut();
    ready_queue_remove(&mut sched.ready_queue, task);
    if sched.current_task == task {
        sched.current_task = ptr::null_mut();
    }
    0
}

fn select_next_task() -> *mut Task {
    let sched = scheduler_mut();
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
    let sched = scheduler_mut();
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
            gdt_set_kernel_rsp0(rsp0);
            context_switch_user(old_ctx_ptr, &(*new_task).context);
        } else {
            gdt_set_kernel_rsp0(&kernel_stack_top as *const u8 as u64);
            if !old_ctx_ptr.is_null() {
                context_switch(old_ctx_ptr, &(*new_task).context);
            } else {
                context_switch(ptr::null_mut(), &(*new_task).context);
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn schedule() {
    let sched = scheduler_mut();
    if sched.enabled == 0 {
        return;
    }
    sched.in_schedule = sched.in_schedule.saturating_add(1);
    sched.schedule_calls = sched.schedule_calls.saturating_add(1);

    let current = sched.current_task;
    if !current.is_null() && current != sched.idle_task {
        if task_is_running(current) {
            if task_set_state(unsafe { (*current).task_id }, TASK_STATE_READY) != 0 {
                unsafe {
                    klog_printf(
                        KlogLevel::Info,
                        b"schedule: failed to mark task ready\n\0".as_ptr() as *const i8,
                    );
                }
            } else if ready_queue_enqueue(&mut sched.ready_queue, current) != 0 {
                unsafe {
                    klog_printf(
                        KlogLevel::Info,
                        b"schedule: ready queue full when re-queuing task\n\0".as_ptr() as *const i8,
                    );
                }
                task_set_state(unsafe { (*current).task_id }, TASK_STATE_RUNNING);
                scheduler_reset_task_quantum(current);
                sched.in_schedule = sched.in_schedule.saturating_sub(1);
                return;
            } else {
                scheduler_reset_task_quantum(current);
            }
        } else if !task_is_blocked(current) && !task_is_terminated(current) {
            unsafe {
                klog_printf(
                    KlogLevel::Info,
                    b"schedule: skipping requeue for task %u\n\0".as_ptr() as *const i8,
                    (*current).task_id,
                );
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

#[no_mangle]
pub extern "C" fn r#yield() {
    let sched = scheduler_mut();
    sched.total_yields += 1;
    if !sched.current_task.is_null() {
        task_record_yield(sched.current_task);
    }
    schedule();
}

#[no_mangle]
pub extern "C" fn block_current_task() {
    let sched = scheduler_mut();
    let current = sched.current_task;
    if current.is_null() {
        return;
    }
    if task_set_state(unsafe { (*current).task_id }, TASK_STATE_BLOCKED) != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"block_current_task: invalid state transition\n\0".as_ptr() as *const i8,
            );
        }
    }
    unschedule_task(current);
    schedule();
}

#[no_mangle]
pub extern "C" fn task_wait_for(task_id: u32) -> c_int {
    let sched = scheduler_mut();
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

#[no_mangle]
pub extern "C" fn unblock_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if task_set_state(unsafe { (*task).task_id }, TASK_STATE_READY) != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"unblock_task: invalid state transition for task %u\n\0".as_ptr() as *const i8,
                (*task).task_id,
            );
        }
    }
    schedule_task(task)
}

#[no_mangle]
pub extern "C" fn scheduler_task_exit() -> ! {
    let sched = scheduler_mut();
    let current = sched.current_task;
    if current.is_null() {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"scheduler_task_exit: No current task\n\0".as_ptr() as *const i8,
            );
        }
        schedule();
        loop {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(current, ptr::null_mut(), timestamp);

    unsafe {
        if crate::task::task_terminate(u32::MAX) != 0 {
            klog_printf(
                KlogLevel::Info,
                b"scheduler_task_exit: Failed to terminate current task\n\0".as_ptr() as *const i8,
            );
        }
    }

    sched.current_task = ptr::null_mut();
    task_set_current(ptr::null_mut());
    schedule();

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"scheduler_task_exit: Schedule returned unexpectedly\n\0".as_ptr() as *const i8,
        );
    }
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

extern "C" fn idle_task_function(_: *mut c_void) {
    let sched = scheduler_mut();
    loop {
        if let Some(cb) = unsafe { IDLE_WAKEUP_CB } {
            if cb() != 0 {
                r#yield();
                continue;
            }
        }
        sched.idle_time = sched.idle_time.saturating_add(1);
        if unsafe { is_kernel_initialized() } != 0 && sched.idle_time > 1000 {
            let mut active_tasks = 0u32;
            unsafe { crate::task::get_task_stats(ptr::null_mut(), &mut active_tasks, ptr::null_mut()) };
            if active_tasks <= 1 {
                break;
            }
        }
        if sched.idle_time % 1000 == 0 {
            r#yield();
        }
    }
    sched.enabled = 0;
}

#[no_mangle]
pub extern "C" fn scheduler_register_idle_wakeup_callback(callback: Option<extern "C" fn() -> c_int>) {
    unsafe {
        IDLE_WAKEUP_CB = callback;
    }
}

#[no_mangle]
pub extern "C" fn init_scheduler() -> c_int {
    let sched = scheduler_mut();
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

#[no_mangle]
pub extern "C" fn create_idle_task() -> c_int {
    let idle_task_id = unsafe {
        crate::task::task_create(
            b"idle\0".as_ptr() as *const i8,
            idle_task_function,
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
    scheduler_mut().idle_task = idle_task;
    0
}

#[no_mangle]
pub extern "C" fn start_scheduler() -> c_int {
    let sched = scheduler_mut();
    if sched.enabled != 0 {
        return -1;
    }
    sched.enabled = 1;
    unsafe { init_kernel_context(&mut sched.return_context) };
    scheduler_set_preemption_enabled(SCHEDULER_PREEMPTION_DEFAULT as c_int);

    if !ready_queue_empty(&sched.ready_queue) {
        schedule();
    } else if !sched.idle_task.is_null() {
        switch_to_task(sched.idle_task);
    } else {
        return -1;
    }
    0
}

#[no_mangle]
pub extern "C" fn stop_scheduler() {
    scheduler_mut().enabled = 0;
}

#[no_mangle]
pub extern "C" fn scheduler_shutdown() {
    let sched = scheduler_mut();
    sched.enabled = 0;
    ready_queue_init(&mut sched.ready_queue);
    sched.current_task = ptr::null_mut();
    sched.idle_task = ptr::null_mut();
}

#[no_mangle]
pub extern "C" fn get_scheduler_stats(
    context_switches: *mut u64,
    yields: *mut u64,
    ready_tasks: *mut u32,
    schedule_calls: *mut u32,
) {
    let sched = scheduler_mut();
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

#[no_mangle]
pub extern "C" fn scheduler_is_enabled() -> c_int {
    scheduler_mut().enabled as c_int
}

#[no_mangle]
pub extern "C" fn scheduler_get_current_task() -> *mut Task {
    scheduler_mut().current_task
}

#[no_mangle]
pub extern "C" fn scheduler_set_preemption_enabled(enabled: c_int) {
    let sched = scheduler_mut();
    sched.preemption_enabled = if enabled != 0 { 1 } else { 0 };
    if sched.preemption_enabled != 0 {
        unsafe { pit_enable_irq() };
    } else {
        sched.reschedule_pending = 0;
        unsafe { pit_disable_irq() };
    }
}

#[no_mangle]
pub extern "C" fn scheduler_is_preemption_enabled() -> c_int {
    scheduler_mut().preemption_enabled as c_int
}

#[no_mangle]
pub extern "C" fn scheduler_timer_tick() {
    let sched = scheduler_mut();
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

#[no_mangle]
pub extern "C" fn scheduler_request_reschedule_from_interrupt() {
    let sched = scheduler_mut();
    if sched.enabled == 0 || sched.preemption_enabled == 0 {
        return;
    }
    if sched.in_schedule == 0 {
        sched.reschedule_pending = 1;
    }
}

#[no_mangle]
pub extern "C" fn scheduler_handle_post_irq() {
    let sched = scheduler_mut();
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

#[no_mangle]
pub extern "C" fn boot_step_task_manager_init() -> c_int {
    unsafe { crate::task::init_task_manager() }
}

#[no_mangle]
pub extern "C" fn boot_step_scheduler_init() -> c_int {
    init_scheduler()
}

#[no_mangle]
pub extern "C" fn boot_step_idle_task() -> c_int {
    create_idle_task()
}

