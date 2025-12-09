#![allow(dead_code)]

use core::ffi::{c_char, c_int, c_void};
use core::mem;
use core::ptr;

use slopos_lib::cpu;
use slopos_lib::kdiag_timestamp;
use slopos_lib::klog::{klog_is_enabled, klog_printf, KlogLevel};

use crate::scheduler;

pub const MAX_TASKS: usize = 32;
pub const TASK_STACK_SIZE: u64 = 0x8000; /* 32KB */
pub const TASK_KERNEL_STACK_SIZE: u64 = 0x8000; /* 32KB */
pub const TASK_NAME_MAX_LEN: usize = 32;
pub const INVALID_TASK_ID: u32 = 0xFFFF_FFFF;
pub const INVALID_PROCESS_ID: u32 = 0xFFFF_FFFF;

pub const TASK_STATE_INVALID: u8 = 0;
pub const TASK_STATE_READY: u8 = 1;
pub const TASK_STATE_RUNNING: u8 = 2;
pub const TASK_STATE_BLOCKED: u8 = 3;
pub const TASK_STATE_TERMINATED: u8 = 4;

pub const TASK_PRIORITY_HIGH: u8 = 0;
pub const TASK_PRIORITY_NORMAL: u8 = 1;
pub const TASK_PRIORITY_LOW: u8 = 2;
pub const TASK_PRIORITY_IDLE: u8 = 3;

pub const TASK_FLAG_USER_MODE: u16 = 0x01;
pub const TASK_FLAG_KERNEL_MODE: u16 = 0x02;
pub const TASK_FLAG_NO_PREEMPT: u16 = 0x04;
pub const TASK_FLAG_SYSTEM: u16 = 0x08;

pub type TaskIterateCb = Option<extern "C" fn(*mut Task, *mut c_void)>;
pub type TaskEntry = extern "C" fn(*mut c_void);

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

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct TaskContext {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    pub cs: u64,
    pub ds: u64,
    pub es: u64,
    pub fs: u64,
    pub gs: u64,
    pub ss: u64,
    pub cr3: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Task {
    pub task_id: u32,
    pub name: [u8; TASK_NAME_MAX_LEN],
    pub state: u8,
    pub priority: u8,
    pub flags: u16,
    pub process_id: u32,
    pub stack_base: u64,
    pub stack_size: u64,
    pub stack_pointer: u64,
    pub kernel_stack_base: u64,
    pub kernel_stack_top: u64,
    pub kernel_stack_size: u64,
    pub entry_point: u64,
    pub entry_arg: *mut c_void,
    pub context: TaskContext,
    pub time_slice: u64,
    pub time_slice_remaining: u64,
    pub total_runtime: u64,
    pub creation_time: u64,
    pub yield_count: u32,
    pub last_run_timestamp: u64,
    pub waiting_on_task_id: u32,
    pub user_started: u8,
    pub context_from_user: u8,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub exit_code: u32,
    pub fate_token: u32,
    pub fate_value: u32,
    pub fate_pending: u8,
    pub next_ready: *mut Task,
}

impl Task {
    const fn invalid() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            name: [0; TASK_NAME_MAX_LEN],
            state: TASK_STATE_INVALID,
            priority: TASK_PRIORITY_NORMAL,
            flags: 0,
            process_id: INVALID_PROCESS_ID,
            stack_base: 0,
            stack_size: 0,
            stack_pointer: 0,
            kernel_stack_base: 0,
            kernel_stack_top: 0,
            kernel_stack_size: 0,
            entry_point: 0,
            entry_arg: ptr::null_mut(),
            context: TaskContext::default(),
            time_slice: 0,
            time_slice_remaining: 0,
            total_runtime: 0,
            creation_time: 0,
            yield_count: 0,
            last_run_timestamp: 0,
            waiting_on_task_id: INVALID_TASK_ID,
            user_started: 0,
            context_from_user: 0,
            exit_reason: TaskExitReason::None,
            fault_reason: TaskFaultReason::None,
            exit_code: 0,
            fate_token: 0,
            fate_value: 0,
            fate_pending: 0,
            next_ready: ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskExitRecord {
    pub task_id: u32,
    pub exit_reason: TaskExitReason,
    pub fault_reason: TaskFaultReason,
    pub exit_code: u32,
}

impl TaskExitRecord {
    const fn empty() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            exit_reason: TaskExitReason::None,
            fault_reason: TaskFaultReason::None,
            exit_code: 0,
        }
    }
}

#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskExitReason {
    None = 0,
    Normal = 1,
    UserFault = 2,
    Kernel = 3,
}

impl Default for TaskExitReason {
    fn default() -> Self {
        TaskExitReason::None
    }
}

#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskFaultReason {
    None = 0,
    UserPage = 1,
    UserGp = 2,
    UserUd = 3,
    UserDeviceNa = 4,
}

impl Default for TaskFaultReason {
    fn default() -> Self {
        TaskFaultReason::None
    }
}

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

extern "C" {
    fn kmalloc(size: usize) -> *mut c_void;
    fn kfree(ptr: *mut c_void);
    fn create_process_vm() -> u32;
    fn destroy_process_vm(process_id: u32) -> c_int;
    fn process_vm_alloc(process_id: u32, size: u64, flags: u32) -> u64;
    fn process_vm_get_page_dir(process_id: u32) -> *mut ProcessPageDir;
    fn fileio_create_table_for_process(process_id: u32) -> c_int;
    fn fileio_destroy_table_for_process(process_id: u32);
    static _user_text_start: u8;
    static _user_text_end: u8;
}

fn task_manager_mut() -> &'static mut TaskManager {
    unsafe { &mut TASK_MANAGER }
}

fn find_task_by_id(task_id: u32) -> *mut Task {
    let mgr = task_manager_mut();
    for task in mgr.tasks.iter_mut() {
        if task.task_id == task_id {
            return task as *mut Task;
        }
    }
    ptr::null_mut()
}

fn find_free_task_slot() -> *mut Task {
    let mgr = task_manager_mut();
    for task in mgr.tasks.iter_mut() {
        if task.state == TASK_STATE_INVALID {
            return task as *mut Task;
        }
    }
    ptr::null_mut()
}

fn release_task_dependents(completed_task_id: u32) {
    let mgr = task_manager_mut();
    for dependent in mgr.tasks.iter_mut() {
        if !task_is_blocked(dependent) {
            continue;
        }
        if dependent.waiting_on_task_id != completed_task_id {
            continue;
        }
        dependent.waiting_on_task_id = INVALID_TASK_ID;
        unsafe {
            if scheduler::unblock_task(dependent as *mut Task) != 0 {
                klog_printf(
                    KlogLevel::Info,
                    b"task_terminate: Failed to unblock dependent task\n\0".as_ptr() as *const c_char,
                );
            }
        }
    }
}

fn user_entry_is_allowed(addr: u64) -> bool {
    let start = unsafe { &_user_text_start as *const u8 as u64 };
    let end = unsafe { &_user_text_end as *const u8 as u64 };
    if start == 0 || end == 0 || start >= end {
        return false;
    }
    addr >= start && addr < end
}

fn task_slot_index(task: *const Task) -> Option<usize> {
    let mgr = task_manager_mut() as *const TaskManager;
    if task.is_null() {
        return None;
    }
    let start = unsafe { &(*mgr).tasks as *const Task as usize };
    let idx = (task as usize - start) / mem::size_of::<Task>();
    if idx < MAX_TASKS {
        Some(idx)
    } else {
        None
    }
}

fn clear_exit_record(task: *const Task) {
    if let Some(idx) = task_slot_index(task) {
        let mgr = task_manager_mut();
        mgr.exit_records[idx] = TaskExitRecord::empty();
    }
}

fn record_task_exit(task: *const Task, exit_reason: TaskExitReason, fault_reason: TaskFaultReason, exit_code: u32) {
    if let Some(idx) = task_slot_index(task) {
        let mgr = task_manager_mut();
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
        task.context.rip = unsafe { task_entry_wrapper as usize as u64 };
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
        task.context.fs = 0;
        task.context.gs = 0;
        task.context.ss = 0x1B;
        task.context.rdi = task.entry_arg as u64;
        task.context.rsi = 0;
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

#[no_mangle]
pub extern "C" fn init_task_manager() -> c_int {
    let mgr = task_manager_mut();
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

#[no_mangle]
pub extern "C" fn task_create(
    name: *const c_char,
    entry_point: TaskEntry,
    arg: *mut c_void,
    priority: u8,
    mut flags: u16,
) -> u32 {
    if entry_point as usize == 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"task_create: Invalid entry point\n\0".as_ptr() as *const c_char,
            );
        }
        return INVALID_TASK_ID;
    }

    if flags & TASK_FLAG_KERNEL_MODE == 0 && flags & TASK_FLAG_USER_MODE == 0 {
        flags |= TASK_FLAG_USER_MODE;
    }

    if flags & TASK_FLAG_KERNEL_MODE != 0 && flags & TASK_FLAG_USER_MODE != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"task_create: Conflicting mode flags\n\0".as_ptr() as *const c_char,
            );
        }
        return INVALID_TASK_ID;
    }

    let mgr = task_manager_mut();

    if mgr.num_tasks >= MAX_TASKS as u32 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"task_create: Maximum tasks reached\n\0".as_ptr() as *const c_char,
            );
        }
        return INVALID_TASK_ID;
    }

    let task = find_free_task_slot();
    if task.is_null() {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"task_create: No free task slots\n\0".as_ptr() as *const c_char,
            );
        }
        return INVALID_TASK_ID;
    }

    clear_exit_record(task);

    let mut process_id = INVALID_PROCESS_ID;
    let mut stack_base = 0u64;
    let mut kernel_stack_base = 0u64;
    let mut kernel_stack_size = 0u64;

    unsafe {
        if flags & TASK_FLAG_KERNEL_MODE != 0 {
            let stack = kmalloc(TASK_STACK_SIZE as usize);
            if stack.is_null() {
                klog_printf(
                    KlogLevel::Info,
                    b"task_create: Failed to allocate kernel stack\n\0".as_ptr() as *const c_char,
                );
                return INVALID_TASK_ID;
            }
            stack_base = stack as u64;
            kernel_stack_base = stack_base;
            kernel_stack_size = TASK_STACK_SIZE;
        } else {
            process_id = create_process_vm();
            if process_id == INVALID_PROCESS_ID {
                klog_printf(
                    KlogLevel::Info,
                    b"task_create: Failed to create process VM\n\0".as_ptr() as *const c_char,
                );
                return INVALID_TASK_ID;
            }

            stack_base = process_vm_alloc(
                process_id,
                TASK_STACK_SIZE,
                (0x1 | 0x2 | 0x4) as u32, /* PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER */
            );

            if stack_base == 0 {
                klog_printf(
                    KlogLevel::Info,
                    b"task_create: Failed to allocate stack\n\0".as_ptr() as *const c_char,
                );
                destroy_process_vm(process_id);
                return INVALID_TASK_ID;
            }

            let kstack = kmalloc(TASK_KERNEL_STACK_SIZE as usize);
            if kstack.is_null() {
                klog_printf(
                    KlogLevel::Info,
                    b"task_create: Failed to allocate kernel RSP0 stack\n\0".as_ptr() as *const c_char,
                );
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
    task_ref.stack_pointer = stack_base + TASK_STACK_SIZE - 16;
    if flags & TASK_FLAG_USER_MODE != 0 && !user_entry_is_allowed(entry_point as u64) {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"task_create: user entry outside user_text window\n\0".as_ptr() as *const c_char,
            );
        }
        if process_id != INVALID_PROCESS_ID {
            unsafe {
                fileio_destroy_table_for_process(process_id);
                destroy_process_vm(process_id);
                if kernel_stack_base != 0 {
                    kfree(kernel_stack_base as *mut c_void);
                }
            }
        } else if kernel_stack_base != 0 {
            unsafe { kfree(kernel_stack_base as *mut c_void) };
        }
        *task_ref = Task::invalid();
        return INVALID_TASK_ID;
    }

    task_ref.kernel_stack_base = kernel_stack_base;
    task_ref.kernel_stack_top = kernel_stack_base + kernel_stack_size;
    task_ref.kernel_stack_size = kernel_stack_size;
    task_ref.entry_point = entry_point as usize as u64;
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
        let page_dir = unsafe { process_vm_get_page_dir(process_id) };
        if !page_dir.is_null() {
            task_ref.context.cr3 = unsafe { (*page_dir).pml4_phys };
        }
    }

    mgr.num_tasks = mgr.num_tasks.saturating_add(1);
    mgr.tasks_created = mgr.tasks_created.saturating_add(1);

    unsafe {
        klog_printf(
            KlogLevel::Debug,
            b"Created task '%s' with ID %u\n\0".as_ptr() as *const c_char,
            task_ref.name.as_ptr(),
            task_id,
        );
    }

    task_id
}

#[no_mangle]
pub extern "C" fn task_terminate(task_id: u32) -> c_int {
    let mut resolved_id = task_id;
    let mut task_ptr: *mut Task = ptr::null_mut();

    if task_id == u32::MAX {
        task_ptr = scheduler::scheduler_get_current_task();
        if task_ptr.is_null() {
            unsafe {
                klog_printf(
                    KlogLevel::Info,
                    b"task_terminate: No current task to terminate\n\0".as_ptr() as *const c_char,
                );
            }
            return -1;
        }
        resolved_id = unsafe { (*task_ptr).task_id };
    } else {
        task_ptr = find_task_by_id(task_id);
    }

    if task_ptr.is_null() || unsafe { (*task_ptr).state } == TASK_STATE_INVALID {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"task_terminate: Task not found\n\0".as_ptr() as *const c_char,
            );
        }
        return -1;
    }

    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"Terminating task '%s' (ID %u)\n\0".as_ptr() as *const c_char,
            (*task_ptr).name.as_ptr(),
            resolved_id,
        );
    }

    let is_current = task_ptr == scheduler::scheduler_get_current_task();

    unsafe {
        scheduler::unschedule_task(task_ptr);
    }

    let now = kdiag_timestamp();
    unsafe {
        if (*task_ptr).last_run_timestamp != 0 && now >= (*task_ptr).last_run_timestamp {
            (*task_ptr).total_runtime += now - (*task_ptr).last_run_timestamp;
        }
        (*task_ptr).last_run_timestamp = 0;
        if (*task_ptr).exit_reason == TaskExitReason::None {
            (*task_ptr).exit_reason = TaskExitReason::Kernel;
        }
        record_task_exit(task_ptr, (*task_ptr).exit_reason, (*task_ptr).fault_reason, (*task_ptr).exit_code);
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

    let mgr = task_manager_mut();
    if !is_current && mgr.num_tasks > 0 {
        mgr.num_tasks -= 1;
    }
    mgr.tasks_terminated = mgr.tasks_terminated.saturating_add(1);

    0
}

#[no_mangle]
pub extern "C" fn task_shutdown_all() -> c_int {
    let mut result = 0;
    let current = scheduler::scheduler_get_current_task();
    for idx in 0..MAX_TASKS {
        let task = unsafe { &mut task_manager_mut().tasks[idx] };
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
    task_manager_mut().num_tasks = 0;
    result
}

#[no_mangle]
pub extern "C" fn task_get_info(task_id: u32, task_info: *mut *mut Task) -> c_int {
    if task_info.is_null() {
        return -1;
    }
    let task = find_task_by_id(task_id);
    unsafe {
        if task.is_null() || (*task).state == TASK_STATE_INVALID {
            *task_info = ptr::null_mut();
            return -1;
        }
        *task_info = task;
    }
    0
}

#[no_mangle]
pub extern "C" fn task_get_exit_record(task_id: u32, record_out: *mut TaskExitRecord) -> c_int {
    if record_out.is_null() {
        return -1;
    }
    let mgr = task_manager_mut();
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
            new_state == TASK_STATE_READY || new_state == TASK_STATE_TERMINATED || new_state == TASK_STATE_BLOCKED
        }
        TASK_STATE_TERMINATED => new_state == TASK_STATE_INVALID || new_state == TASK_STATE_TERMINATED,
        _ => false,
    }
}

#[no_mangle]
pub extern "C" fn task_set_state(task_id: u32, new_state: u8) -> c_int {
    let task = find_task_by_id(task_id);
    if task.is_null() || unsafe { (*task).state } == TASK_STATE_INVALID {
        return -1;
    }

    let old_state = unsafe { (*task).state };
    if !task_state_transition_allowed(old_state, new_state) {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"task_set_state: invalid transition for task %u\n\0".as_ptr() as *const c_char,
                task_id,
            );
        }
    }

    unsafe { (*task).state = new_state };

    if unsafe { klog_is_enabled(KlogLevel::Debug) } != 0 {
        unsafe {
            klog_printf(
                KlogLevel::Debug,
                b"Task %u state: %u -> %u\n\0".as_ptr() as *const c_char,
                task_id,
                old_state,
                new_state,
            );
        }
    }

    0
}

#[no_mangle]
pub extern "C" fn get_task_stats(
    total_tasks: *mut u32,
    active_tasks: *mut u32,
    context_switches: *mut u64,
) {
    let mgr = task_manager_mut();
    unsafe {
        if !total_tasks.is_null() {
            *total_tasks = mgr.tasks_created;
        }
        if !active_tasks.is_null() {
            *active_tasks = mgr.num_tasks;
        }
        if !context_switches.is_null() {
            *context_switches = mgr.total_context_switches;
        }
    }
}

#[no_mangle]
pub extern "C" fn task_record_context_switch(from: *mut Task, to: *mut Task, timestamp: u64) {
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
        task_manager_mut().total_context_switches += 1;
    }
}

#[no_mangle]
pub extern "C" fn task_record_yield(task: *mut Task) {
    task_manager_mut().total_yields += 1;
    if !task.is_null() {
        unsafe { (*task).yield_count = (*task).yield_count.saturating_add(1) };
    }
}

#[no_mangle]
pub extern "C" fn task_get_total_yields() -> u64 {
    task_manager_mut().total_yields
}

#[no_mangle]
pub extern "C" fn task_state_to_string(state: u8) -> *const c_char {
    match state {
        TASK_STATE_INVALID => b"invalid\0".as_ptr() as *const c_char,
        TASK_STATE_READY => b"ready\0".as_ptr() as *const c_char,
        TASK_STATE_RUNNING => b"running\0".as_ptr() as *const c_char,
        TASK_STATE_BLOCKED => b"blocked\0".as_ptr() as *const c_char,
        TASK_STATE_TERMINATED => b"terminated\0".as_ptr() as *const c_char,
        _ => b"unknown\0".as_ptr() as *const c_char,
    }
}

#[no_mangle]
pub extern "C" fn task_iterate_active(callback: TaskIterateCb, context: *mut c_void) {
    if callback.is_none() {
        return;
    }
    let cb = callback.unwrap();
    let mgr = task_manager_mut();
    for task in mgr.tasks.iter_mut() {
        if task.state == TASK_STATE_INVALID || task.task_id == INVALID_TASK_ID {
            continue;
        }
        cb(task as *mut Task, context);
    }
}

#[no_mangle]
pub extern "C" fn task_get_current_id() -> u32 {
    let current = scheduler::scheduler_get_current_task();
    if current.is_null() {
        0
    } else {
        unsafe { (*current).task_id }
    }
}

#[no_mangle]
pub extern "C" fn task_get_current() -> *mut Task {
    scheduler::scheduler_get_current_task()
}

#[no_mangle]
pub extern "C" fn task_set_current(task: *mut Task) {
    if task.is_null() {
        return;
    }
    unsafe {
        if (*task).state != TASK_STATE_READY && (*task).state != TASK_STATE_RUNNING {
            klog_printf(
                KlogLevel::Info,
                b"task_set_current: unexpected state transition\n\0".as_ptr() as *const c_char,
            );
        }
        (*task).state = TASK_STATE_RUNNING;
    }
}

#[no_mangle]
pub extern "C" fn task_get_state(task: *const Task) -> u8 {
    if task.is_null() {
        return TASK_STATE_INVALID;
    }
    unsafe { (*task).state }
}

#[no_mangle]
pub extern "C" fn task_is_ready(task: *const Task) -> bool {
    task_get_state(task) == TASK_STATE_READY
}

#[no_mangle]
pub extern "C" fn task_is_running(task: *const Task) -> bool {
    task_get_state(task) == TASK_STATE_RUNNING
}

#[no_mangle]
pub extern "C" fn task_is_blocked(task: *const Task) -> bool {
    task_get_state(task) == TASK_STATE_BLOCKED
}

#[no_mangle]
pub extern "C" fn task_is_terminated(task: *const Task) -> bool {
    task_get_state(task) == TASK_STATE_TERMINATED
}

extern "C" {
    fn task_entry_wrapper();
}

