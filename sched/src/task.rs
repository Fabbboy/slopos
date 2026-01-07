use core::ffi::{c_char, c_int, c_void};
use core::mem;
use core::ptr;

use slopos_lib::cpu;
use slopos_lib::kdiag_timestamp;
use slopos_lib::{klog_debug, klog_info};

use crate::scheduler;

// Re-export all task types and constants from abi
pub use slopos_abi::task::{
    IdtEntry, Task, TaskContext, TaskExitReason, TaskExitRecord, TaskFaultReason,
    INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, TASK_FLAG_COMPOSITOR,
    TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM,
    TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN, TASK_PRIORITY_HIGH,
    TASK_PRIORITY_IDLE, TASK_PRIORITY_LOW, TASK_PRIORITY_NORMAL, TASK_STACK_SIZE,
    TASK_STATE_BLOCKED, TASK_STATE_INVALID, TASK_STATE_READY, TASK_STATE_RUNNING,
    TASK_STATE_TERMINATED,
};

const USER_CODE_BASE: u64 = 0x0000_0000_0040_0000;

pub type TaskIterateCb = Option<fn(*mut Task, *mut c_void)>;
pub type TaskEntry = fn(*mut c_void);

// Task::invalid(), TaskExitRecord::empty() and all types are now in abi

struct TaskManager {
    tasks: [Task; MAX_TASKS],
    num_tasks: u32,
    next_task_id: u32,
    total_context_switches: u64,
    total_yields: u64,
    tasks_created: u32,
    tasks_terminated: u32,
    exit_records: [TaskExitRecord; MAX_TASKS],
}

impl TaskManager {
    const fn new() -> Self {
        Self {
            tasks: [Task::invalid(); MAX_TASKS],
            num_tasks: 0,
            next_task_id: 1,
            total_context_switches: 0,
            total_yields: 0,
            tasks_created: 0,
            tasks_terminated: 0,
            exit_records: [TaskExitRecord::empty(); MAX_TASKS],
        }
    }
}

static mut TASK_MANAGER: TaskManager = TaskManager::new();

use slopos_fs::fileio::{fileio_create_table_for_process, fileio_destroy_table_for_process};
use slopos_mm::kernel_heap::{kfree, kmalloc};
use slopos_mm::process_vm::{
    create_process_vm, destroy_process_vm, process_vm_alloc, process_vm_get_page_dir,
};
use slopos_mm::shared_memory::shm_cleanup_task;
use slopos_mm::symbols;
use slopos_drivers::sched_bridge;

fn task_manager_mut() -> *mut TaskManager {
    &raw mut TASK_MANAGER
}

pub fn task_find_by_id(task_id: u32) -> *mut Task {
    let mgr = unsafe { &mut *task_manager_mut() };
    for task in mgr.tasks.iter_mut() {
        if task.task_id == task_id {
            return task as *mut Task;
        }
    }
    ptr::null_mut()
}

fn find_free_task_slot() -> *mut Task {
    let mgr = unsafe { &mut *task_manager_mut() };
    for task in mgr.tasks.iter_mut() {
        if task.state == TASK_STATE_INVALID {
            return task as *mut Task;
        }
    }
    ptr::null_mut()
}

fn release_task_dependents(completed_task_id: u32) {
    let mgr = unsafe { &mut *task_manager_mut() };
    for dependent in mgr.tasks.iter_mut() {
        if !task_is_blocked(dependent) {
            continue;
        }
        if dependent.waiting_on_task_id != completed_task_id {
            continue;
        }
        dependent.waiting_on_task_id = INVALID_TASK_ID;
        if scheduler::unblock_task(dependent as *mut Task) != 0 {
            klog_info!("task_terminate: Failed to unblock dependent task");
        }
    }
}

fn user_entry_is_allowed(addr: u64) -> bool {
    // Allow entry points in embedded user_text section (for legacy compatibility)
    let (start_ptr, end_ptr) = symbols::user_text_bounds();
    let start = start_ptr as u64;
    let end = end_ptr as u64;
    if start != 0 && end != 0 && start < end && addr >= start && addr < end {
        return true;
    }
    // Allow entry points in PROCESS_CODE_START_VA range (for ELF binaries)
    // ELF binaries are loaded at 0x400000, allow a reasonable range
    const PROCESS_CODE_START: u64 = 0x0000_0000_0040_0000;
    const PROCESS_CODE_END: u64 = 0x0000_0000_0050_0000; // 1MB range
    addr >= PROCESS_CODE_START && addr < PROCESS_CODE_END
}

fn task_slot_index(task: *const Task) -> Option<usize> {
    let mgr = task_manager_mut() as *const TaskManager;
    if task.is_null() {
        return None;
    }
    let start = unsafe { &(*mgr).tasks as *const Task as usize };
    let idx = (task as usize - start) / mem::size_of::<Task>();
    if idx < MAX_TASKS { Some(idx) } else { None }
}

fn clear_exit_record(task: *const Task) {
    if let Some(idx) = task_slot_index(task) {
        let mgr = unsafe { &mut *task_manager_mut() };
        mgr.exit_records[idx] = TaskExitRecord::empty();
    }
}

fn record_task_exit(
    task: *const Task,
    exit_reason: TaskExitReason,
    fault_reason: TaskFaultReason,
    exit_code: u32,
) {
    if let Some(idx) = task_slot_index(task) {
        let mgr = unsafe { &mut *task_manager_mut() };
        mgr.exit_records[idx] = TaskExitRecord {
            task_id: unsafe { (*task).task_id },
            exit_reason,
            fault_reason,
            exit_code,
        };
    }
}

fn init_task_context(task: &mut Task) {
    task.context = TaskContext::default();
    task.context.rsi = task.entry_arg as u64;
    task.context.rdi = task.entry_point;
    task.context.rsp = task.stack_pointer;
    task.context.rflags = 0x202;

    if task.flags & TASK_FLAG_KERNEL_MODE != 0 {
        task.context.rip = task_entry_wrapper as *const () as usize as u64;
    } else {
        task.context.rip = task.entry_point;
    }

    if task.flags & TASK_FLAG_KERNEL_MODE != 0 {
        task.context.cs = 0x08;
        task.context.ds = 0x10;
        task.context.es = 0x10;
        task.context.fs = 0;
        task.context.gs = 0;
        task.context.ss = 0x10;
    } else {
        task.context.cs = 0x23;
        task.context.ds = 0x1B;
        task.context.es = 0x1B;
        task.context.fs = 0x1B;
        task.context.gs = 0x1B;
        task.context.ss = 0x1B;
        task.context.rdi = task.entry_arg as u64;
        task.context.rsi = 0;
        // #region agent log
        {
            use slopos_lib::klog_info;
            let rip = task.context.rip;
            let rsp = task.context.rsp;
            let rdi = task.context.rdi;
            let entry_point = task.entry_point;
            klog_info!("init_task_context: user task rip=0x{:x} rsp=0x{:x} rdi=0x{:x} entry_point=0x{:x}\n", rip, rsp, rdi, entry_point);
        }
        // #endregion
    }

    task.context.cr3 = 0;
}

unsafe fn copy_name(dest: &mut [u8; TASK_NAME_MAX_LEN], src: *const c_char) {
    if src.is_null() {
        dest[0] = 0;
        return;
    }
    let mut i = 0;
    while i < TASK_NAME_MAX_LEN - 1 {
        let ch = unsafe { *src.add(i) };
        if ch == 0 {
            break;
        }
        dest[i] = ch as u8;
        i += 1;
    }
    dest[i] = 0;
    while i + 1 < TASK_NAME_MAX_LEN {
        i += 1;
        dest[i] = 0;
    }
}
pub fn init_task_manager() -> c_int {
    let mgr = unsafe { &mut *task_manager_mut() };
    mgr.num_tasks = 0;
    mgr.next_task_id = 1;
    mgr.total_context_switches = 0;
    mgr.total_yields = 0;
    mgr.tasks_created = 0;
    mgr.tasks_terminated = 0;
    for task in mgr.tasks.iter_mut() {
        *task = Task::invalid();
    }
    for rec in mgr.exit_records.iter_mut() {
        *rec = TaskExitRecord::empty();
    }
    0
}
pub fn task_create(
    name: *const c_char,
    entry_point: TaskEntry,
    arg: *mut c_void,
    priority: u8,
    mut flags: u16,
) -> u32 {
    if entry_point as usize == 0 {
        klog_info!("task_create: Invalid entry point");
        return INVALID_TASK_ID;
    }

    if flags & TASK_FLAG_KERNEL_MODE == 0 && flags & TASK_FLAG_USER_MODE == 0 {
        flags |= TASK_FLAG_USER_MODE;
    }

    if flags & TASK_FLAG_KERNEL_MODE != 0 && flags & TASK_FLAG_USER_MODE != 0 {
        klog_info!("task_create: Conflicting mode flags");
        return INVALID_TASK_ID;
    }

    let mgr = unsafe { &mut *task_manager_mut() };

    if mgr.num_tasks >= MAX_TASKS as u32 {
        klog_info!("task_create: Maximum tasks reached");
        return INVALID_TASK_ID;
    }

    let task = find_free_task_slot();
    if task.is_null() {
        klog_info!("task_create: No free task slots");
        return INVALID_TASK_ID;
    }

    clear_exit_record(task);

    let mut process_id = INVALID_PROCESS_ID;
    let stack_base;
    let kernel_stack_base;
    let kernel_stack_size;

    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        let stack = kmalloc(TASK_STACK_SIZE as usize);
        if stack.is_null() {
            klog_info!("task_create: Failed to allocate kernel stack");
            return INVALID_TASK_ID;
        }
        stack_base = stack as u64;
        kernel_stack_base = stack_base;
        kernel_stack_size = TASK_STACK_SIZE;
    } else {
        process_id = create_process_vm();
        if process_id == INVALID_PROCESS_ID {
            klog_info!("task_create: Failed to create process VM");
            return INVALID_TASK_ID;
        }

        stack_base = process_vm_alloc(
            process_id,
            TASK_STACK_SIZE,
            (0x1 | 0x2 | 0x4) as u32, /* PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER */
        );

        if stack_base == 0 {
            klog_info!("task_create: Failed to allocate stack");
            destroy_process_vm(process_id);
            return INVALID_TASK_ID;
        }

        let kstack = kmalloc(TASK_KERNEL_STACK_SIZE as usize);
        if kstack.is_null() {
            klog_info!("task_create: Failed to allocate kernel RSP0 stack");
            destroy_process_vm(process_id);
            return INVALID_TASK_ID;
        }

        kernel_stack_base = kstack as u64;
        kernel_stack_size = TASK_KERNEL_STACK_SIZE;

        if fileio_create_table_for_process(process_id) != 0 {
            kfree(kstack);
            destroy_process_vm(process_id);
            return INVALID_TASK_ID;
        }
    }

    let task_id = {
        let next = mgr.next_task_id;
        mgr.next_task_id = next.wrapping_add(1);
        next
    };

    let task_ref = unsafe { &mut *task };
    task_ref.task_id = task_id;
    unsafe { copy_name(&mut task_ref.name, name) };
    task_ref.state = TASK_STATE_READY;
    task_ref.priority = priority;
    task_ref.flags = flags;
    task_ref.process_id = process_id;
    task_ref.stack_base = stack_base;
    task_ref.stack_size = TASK_STACK_SIZE;
    // Align for System V ABI expectations (rsp is 16-byte aligned before CALL, 8-byte inside).
    task_ref.stack_pointer = stack_base + TASK_STACK_SIZE - 8;
    if flags & TASK_FLAG_USER_MODE != 0 && !user_entry_is_allowed(entry_point as u64) {
        klog_info!("task_create: user entry outside user_text window");
        if process_id != INVALID_PROCESS_ID {
            fileio_destroy_table_for_process(process_id);
            destroy_process_vm(process_id);
            if kernel_stack_base != 0 {
                kfree(kernel_stack_base as *mut c_void);
            }
        } else if kernel_stack_base != 0 {
            kfree(kernel_stack_base as *mut c_void);
        }
        *task_ref = Task::invalid();
        return INVALID_TASK_ID;
    }

    task_ref.kernel_stack_base = kernel_stack_base;
    task_ref.kernel_stack_top = kernel_stack_base + kernel_stack_size;
    task_ref.kernel_stack_size = kernel_stack_size;
    if flags & TASK_FLAG_USER_MODE != 0 {
        let entry_addr = entry_point as u64;
        let (text_start, text_end) = slopos_mm::symbols::user_text_bounds();
        let text_start = text_start as u64;
        let text_end = text_end as u64;
        if entry_addr >= text_start && entry_addr < text_end {
            // Align to page boundaries to match map_user_sections behavior
            use slopos_mm::mm_constants::PAGE_SIZE_4KB;
            use slopos_lib::align_down;
            let text_start_aligned = align_down(text_start as usize, PAGE_SIZE_4KB as usize) as u64;
            // Calculate offset from aligned start to match map_user_sections mapping
            let offset = entry_addr - text_start_aligned;
            task_ref.entry_point = USER_CODE_BASE + offset;
        } else {
            task_ref.entry_point = entry_addr;
        }
    } else {
        task_ref.entry_point = entry_point as usize as u64;
    }
    task_ref.entry_arg = arg;
    task_ref.time_slice = 10;
    task_ref.time_slice_remaining = task_ref.time_slice;
    task_ref.total_runtime = 0;
    task_ref.creation_time = kdiag_timestamp();
    task_ref.yield_count = 0;
    task_ref.last_run_timestamp = 0;
    task_ref.waiting_on_task_id = INVALID_TASK_ID;
    task_ref.user_started = 0;
    task_ref.context_from_user = 0;
    task_ref.exit_reason = TaskExitReason::None;
    task_ref.fault_reason = TaskFaultReason::None;
    task_ref.exit_code = 0;
    task_ref.fate_token = 0;
    task_ref.fate_value = 0;
    task_ref.fate_pending = 0;
    task_ref.next_ready = ptr::null_mut();

    init_task_context(task_ref);

    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        task_ref.context.cr3 = cpu::read_cr3() & !0xFFF;
    } else {
        let page_dir = process_vm_get_page_dir(process_id);
        if !page_dir.is_null() {
            task_ref.context.cr3 = unsafe { (*page_dir).pml4_phys };
        }
    }

    mgr.num_tasks = mgr.num_tasks.saturating_add(1);
    mgr.tasks_created = mgr.tasks_created.saturating_add(1);

    unsafe {
        use core::ffi::CStr;
        let name_str = CStr::from_ptr(task_ref.name.as_ptr() as *const c_char)
            .to_str()
            .unwrap_or("<invalid utf-8>");
        klog_debug!("Created task '{}' with ID {}", name_str, task_id);
    }

    task_id
}
pub fn task_terminate(task_id: u32) -> c_int {
    let mut resolved_id = task_id;
    let task_ptr: *mut Task;

    if task_id == u32::MAX {
        task_ptr = scheduler::scheduler_get_current_task();
        if task_ptr.is_null() {
            klog_info!("task_terminate: No current task to terminate");
            return -1;
        }
        resolved_id = unsafe { (*task_ptr).task_id };
    } else {
        task_ptr = task_find_by_id(task_id);
    }

    if task_ptr.is_null() || unsafe { (*task_ptr).state } == TASK_STATE_INVALID {
        klog_info!("task_terminate: Task not found");
        return -1;
    }

    unsafe {
        use core::ffi::CStr;
        let name_str = CStr::from_ptr((*task_ptr).name.as_ptr() as *const c_char)
            .to_str()
            .unwrap_or("<invalid utf-8>");
        klog_info!("Terminating task '{}' (ID {})", name_str, resolved_id);
    }

    let is_current = task_ptr == scheduler::scheduler_get_current_task();

    scheduler::unschedule_task(task_ptr);

    let now = kdiag_timestamp();
    unsafe {
        if (*task_ptr).last_run_timestamp != 0 && now >= (*task_ptr).last_run_timestamp {
            (*task_ptr).total_runtime += now - (*task_ptr).last_run_timestamp;
        }
        (*task_ptr).last_run_timestamp = 0;
        if (*task_ptr).exit_reason == TaskExitReason::None {
            (*task_ptr).exit_reason = TaskExitReason::Kernel;
        }
        record_task_exit(
            task_ptr,
            (*task_ptr).exit_reason,
            (*task_ptr).fault_reason,
            (*task_ptr).exit_code,
        );
        (*task_ptr).state = TASK_STATE_TERMINATED;
        (*task_ptr).fate_token = 0;
        (*task_ptr).fate_value = 0;
        (*task_ptr).fate_pending = 0;
    }

    release_task_dependents(resolved_id);

    if !is_current {
        unsafe {
            if (*task_ptr).process_id != INVALID_PROCESS_ID {
                fileio_destroy_table_for_process((*task_ptr).process_id);
                // Clean up video/surface resources for this task
                sched_bridge::video_task_cleanup(resolved_id);
                // Clean up shared memory buffers owned by this task
                // Must happen before destroy_process_vm to properly unmap pages
                shm_cleanup_task(resolved_id);
                destroy_process_vm((*task_ptr).process_id);
                if (*task_ptr).kernel_stack_base != 0 {
                    kfree((*task_ptr).kernel_stack_base as *mut c_void);
                }
            } else if (*task_ptr).stack_base != 0 {
                kfree((*task_ptr).stack_base as *mut c_void);
            }
            *task_ptr = Task::invalid();
        }
    }

    let mgr = unsafe { &mut *task_manager_mut() };
    if !is_current && mgr.num_tasks > 0 {
        mgr.num_tasks -= 1;
    }
    mgr.tasks_terminated = mgr.tasks_terminated.saturating_add(1);

    0
}
pub fn task_shutdown_all() -> c_int {
    let mut result = 0;
    let current = scheduler::scheduler_get_current_task();
    for idx in 0..MAX_TASKS {
        let task = unsafe { &mut (*task_manager_mut()).tasks[idx] };
        if task.state == TASK_STATE_INVALID {
            continue;
        }
        if (task as *mut Task) == current {
            continue;
        }
        if task.task_id == INVALID_TASK_ID {
            continue;
        }
        if task_terminate(task.task_id) != 0 {
            result = -1;
        }
    }
    unsafe { (*task_manager_mut()).num_tasks = 0 };
    result
}
pub fn task_get_info(task_id: u32, task_info: *mut *mut Task) -> c_int {
    if task_info.is_null() {
        return -1;
    }
    let task = task_find_by_id(task_id);
    unsafe {
        if task.is_null() || (*task).state == TASK_STATE_INVALID {
            *task_info = ptr::null_mut();
            return -1;
        }
        *task_info = task;
    }
    0
}
pub fn task_get_exit_record(task_id: u32, record_out: *mut TaskExitRecord) -> c_int {
    if record_out.is_null() {
        return -1;
    }
    let mgr = unsafe { &mut *task_manager_mut() };
    for rec in mgr.exit_records.iter() {
        if rec.task_id == task_id {
            unsafe { *record_out = *rec };
            return 0;
        }
    }
    -1
}

fn task_state_transition_allowed(old_state: u8, new_state: u8) -> bool {
    if old_state == new_state {
        return true;
    }
    match old_state {
        TASK_STATE_INVALID => new_state == TASK_STATE_READY || new_state == TASK_STATE_INVALID,
        TASK_STATE_READY => {
            new_state == TASK_STATE_RUNNING
                || new_state == TASK_STATE_BLOCKED
                || new_state == TASK_STATE_TERMINATED
                || new_state == TASK_STATE_READY
        }
        TASK_STATE_RUNNING => {
            new_state == TASK_STATE_READY
                || new_state == TASK_STATE_BLOCKED
                || new_state == TASK_STATE_TERMINATED
        }
        TASK_STATE_BLOCKED => {
            new_state == TASK_STATE_READY
                || new_state == TASK_STATE_TERMINATED
                || new_state == TASK_STATE_BLOCKED
        }
        TASK_STATE_TERMINATED => {
            new_state == TASK_STATE_INVALID || new_state == TASK_STATE_TERMINATED
        }
        _ => false,
    }
}
pub fn task_set_state(task_id: u32, new_state: u8) -> c_int {
    let task = task_find_by_id(task_id);
    if task.is_null() || unsafe { (*task).state } == TASK_STATE_INVALID {
        return -1;
    }

    let old_state = unsafe { (*task).state };
    if !task_state_transition_allowed(old_state, new_state) {
        klog_info!("task_set_state: invalid transition for task {}", task_id);
    }

    unsafe { (*task).state = new_state };

    0
}
pub fn get_task_stats(total_tasks: *mut u32, active_tasks: *mut u32, context_switches: *mut u64) {
    let mgr = unsafe { &mut *task_manager_mut() };
    if !total_tasks.is_null() {
        unsafe { *total_tasks = mgr.tasks_created };
    }
    if !active_tasks.is_null() {
        unsafe { *active_tasks = mgr.num_tasks };
    }
    if !context_switches.is_null() {
        unsafe { *context_switches = mgr.total_context_switches };
    }
}
pub fn task_record_context_switch(from: *mut Task, to: *mut Task, timestamp: u64) {
    if !from.is_null() {
        unsafe {
            if (*from).last_run_timestamp != 0 && timestamp >= (*from).last_run_timestamp {
                (*from).total_runtime += timestamp - (*from).last_run_timestamp;
            }
            (*from).last_run_timestamp = 0;
        }
    }

    if !to.is_null() {
        unsafe { (*to).last_run_timestamp = timestamp };
    }

    if !to.is_null() && to != from {
        unsafe { (*task_manager_mut()).total_context_switches += 1 };
    }
}
pub fn task_record_yield(task: *mut Task) {
    unsafe { (*task_manager_mut()).total_yields += 1 };
    if !task.is_null() {
        unsafe { (*task).yield_count = (*task).yield_count.saturating_add(1) };
    }
}
pub fn task_get_total_yields() -> u64 {
    unsafe { (*task_manager_mut()).total_yields }
}
pub fn task_state_to_string(state: u8) -> *const c_char {
    match state {
        TASK_STATE_INVALID => b"invalid\0".as_ptr() as *const c_char,
        TASK_STATE_READY => b"ready\0".as_ptr() as *const c_char,
        TASK_STATE_RUNNING => b"running\0".as_ptr() as *const c_char,
        TASK_STATE_BLOCKED => b"blocked\0".as_ptr() as *const c_char,
        TASK_STATE_TERMINATED => b"terminated\0".as_ptr() as *const c_char,
        _ => b"unknown\0".as_ptr() as *const c_char,
    }
}
pub fn task_iterate_active(callback: TaskIterateCb, context: *mut c_void) {
    if callback.is_none() {
        return;
    }
    let cb = callback.unwrap();
    let mgr = unsafe { &mut *task_manager_mut() };
    for task in mgr.tasks.iter_mut() {
        if task.state == TASK_STATE_INVALID || task.task_id == INVALID_TASK_ID {
            continue;
        }
        cb(task as *mut Task, context);
    }
}
pub fn task_get_current_id() -> u32 {
    let current = scheduler::scheduler_get_current_task();
    if current.is_null() {
        0
    } else {
        unsafe { (*current).task_id }
    }
}
pub fn task_get_current() -> *mut Task {
    scheduler::scheduler_get_current_task()
}
pub fn task_set_current(task: *mut Task) {
    if task.is_null() {
        return;
    }
    unsafe {
        if (*task).state != TASK_STATE_READY && (*task).state != TASK_STATE_RUNNING {
            klog_info!("task_set_current: unexpected state transition");
        }
        (*task).state = TASK_STATE_RUNNING;
    }
}
pub fn task_get_state(task: *const Task) -> u8 {
    if task.is_null() {
        return TASK_STATE_INVALID;
    }
    unsafe { (*task).state }
}
pub fn task_is_ready(task: *const Task) -> bool {
    task_get_state(task) == TASK_STATE_READY
}
pub fn task_is_running(task: *const Task) -> bool {
    task_get_state(task) == TASK_STATE_RUNNING
}
pub fn task_is_blocked(task: *const Task) -> bool {
    task_get_state(task) == TASK_STATE_BLOCKED
}
pub fn task_is_terminated(task: *const Task) -> bool {
    task_get_state(task) == TASK_STATE_TERMINATED
}

use crate::ffi_boundary::task_entry_wrapper;
