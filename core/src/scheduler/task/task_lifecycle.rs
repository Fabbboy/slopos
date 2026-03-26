use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_arch::cpu;
use slopos_utils::kdiag_timestamp;
use slopos_utils::string::bytes_as_str;
use slopos_utils::{klog_debug, klog_info};

use super::super::ffi_boundary::task_entry_wrapper;
use super::super::scheduler;
use super::task_cleanup_hooks::run_task_resource_cleanup_hooks;
use super::task_session::{notify_parent_of_child_exit, release_task_dependents};
use super::task_stats::{record_task_created, record_task_exit};
use super::task_table::{
    ReserveTaskSlotError, defer_task_cleanup, free_task_stacks, release_task_slot,
    reserve_task_slot, task_find_by_id, with_task_manager,
};
use super::{
    FpuState, INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, TASK_FLAG_KERNEL_MODE,
    TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN, TASK_STACK_SIZE, Task,
    TaskContext, TaskEntry, TaskExitReason, TaskFaultReason, TaskStatus,
};
use slopos_fs::fileio::{
    fileio_clone_table_for_process, fileio_create_table_for_process,
    fileio_destroy_table_for_process,
};
use slopos_kernel_services::syscall_services::tty;
use slopos_mm::kernel_heap::{kfree, kmalloc};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::process_vm::{
    create_process_vm, destroy_process_vm, process_vm_clone_cow, process_vm_get_page_dir,
    process_vm_get_stack_top,
};
use slopos_mm::shared_memory::shm_cleanup_task;
use slopos_mm::user_copy::copy_to_user;
use slopos_mm::user_ptr::UserPtr;

fn user_entry_is_allowed(addr: u64) -> bool {
    const PROCESS_CODE_END: u64 = 0x0000_0000_0050_0000;
    addr >= PROCESS_CODE_START_VA && addr < PROCESS_CODE_END
}

struct TaskCreateResources {
    process_id: u32,
    stack_base: u64,
    kernel_stack_base: u64,
    kernel_stack_size: u64,
}

struct ProcessResourceLease {
    process_id: u32,
    owns_vm: bool,
    owns_file_table: bool,
}

impl ProcessResourceLease {
    const fn none() -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            owns_vm: false,
            owns_file_table: false,
        }
    }

    #[inline]
    const fn process_id(&self) -> u32 {
        self.process_id
    }

    fn create_user_process() -> Option<Self> {
        let process_id = create_process_vm();
        if process_id == INVALID_PROCESS_ID {
            klog_info!("task_create: Failed to create process VM");
            return None;
        }

        if fileio_create_table_for_process(process_id) != 0 {
            destroy_process_vm(process_id);
            return None;
        }

        Some(Self {
            process_id,
            owns_vm: true,
            owns_file_table: true,
        })
    }

    fn clone_from_parent(parent_process_id: u32) -> Option<Self> {
        let child_process_id = process_vm_clone_cow(parent_process_id);
        if child_process_id == INVALID_PROCESS_ID {
            return None;
        }

        if fileio_clone_table_for_process(parent_process_id, child_process_id) != 0 {
            destroy_process_vm(child_process_id);
            return None;
        }

        Some(Self {
            process_id: child_process_id,
            owns_vm: true,
            owns_file_table: true,
        })
    }

    fn disarm(mut self) -> u32 {
        let process_id = self.process_id;
        self.process_id = INVALID_PROCESS_ID;
        self.owns_vm = false;
        self.owns_file_table = false;
        process_id
    }

    fn cleanup_owned_process(process_id: u32, owns_vm: bool, owns_file_table: bool) {
        if process_id == INVALID_PROCESS_ID {
            return;
        }

        if owns_file_table {
            fileio_destroy_table_for_process(process_id);
        }
        if owns_vm {
            destroy_process_vm(process_id);
        }
    }
}

impl Drop for ProcessResourceLease {
    fn drop(&mut self) {
        Self::cleanup_owned_process(self.process_id, self.owns_vm, self.owns_file_table);
    }
}

struct KernelStackLease {
    base: *mut c_void,
}

impl KernelStackLease {
    fn allocate(size: u64, failure_message: &'static str) -> Option<Self> {
        let stack = kmalloc(size as usize);
        if stack.is_null() {
            klog_info!("{}", failure_message);
            return None;
        }
        Some(Self { base: stack })
    }

    #[inline]
    fn base_u64(&self) -> u64 {
        self.base as u64
    }

    fn disarm(mut self) -> *mut c_void {
        let base = self.base;
        self.base = ptr::null_mut();
        base
    }
}

impl Drop for KernelStackLease {
    fn drop(&mut self) {
        if !self.base.is_null() {
            kfree(self.base);
            self.base = ptr::null_mut();
        }
    }
}

fn allocate_kernel_task_resources() -> Option<TaskCreateResources> {
    let stack = KernelStackLease::allocate(
        TASK_STACK_SIZE,
        "task_create: Failed to allocate kernel stack",
    )?;
    let stack_base = stack.disarm() as u64;
    Some(TaskCreateResources {
        process_id: INVALID_PROCESS_ID,
        stack_base,
        kernel_stack_base: stack_base,
        kernel_stack_size: TASK_STACK_SIZE,
    })
}

fn allocate_user_task_resources() -> Option<TaskCreateResources> {
    let process = ProcessResourceLease::create_user_process()?;
    let process_id = process.process_id();

    let stack_top = process_vm_get_stack_top(process_id);
    if stack_top == 0 {
        klog_info!("task_create: Failed to get process stack");
        return None;
    }

    let kstack = KernelStackLease::allocate(
        TASK_KERNEL_STACK_SIZE,
        "task_create: Failed to allocate kernel RSP0 stack",
    )?;

    Some(TaskCreateResources {
        process_id: process.disarm(),
        stack_base: stack_top - TASK_STACK_SIZE,
        kernel_stack_base: kstack.disarm() as u64,
        kernel_stack_size: TASK_KERNEL_STACK_SIZE,
    })
}

fn allocate_task_create_resources(flags: u16) -> Option<TaskCreateResources> {
    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        allocate_kernel_task_resources()
    } else {
        allocate_user_task_resources()
    }
}

fn cleanup_task_create_resources(process_id: u32, kernel_stack_base: u64) {
    ProcessResourceLease::cleanup_owned_process(process_id, true, true);

    if kernel_stack_base != 0 {
        kfree(kernel_stack_base as *mut c_void);
    }
}

fn reset_task_runtime_fields(task: &mut Task) {
    task.time_slice_remaining = task.time_slice;
    task.total_runtime = 0;
    task.creation_time = kdiag_timestamp();
    task.yield_count = 0;
    task.last_run_timestamp = 0;
    task.waiting_on.store(INVALID_TASK_ID, Ordering::Release);
    task.exit_reason = TaskExitReason::None;
    task.fault_reason = TaskFaultReason::None;
    task.exit_code = 0;
    task.fate_token = 0;
    task.fate_value = 0;
    task.fate_pending = 0;
    task.next_ready = ptr::null_mut();
    task.next_inbox.store(ptr::null_mut(), Ordering::Release);
    task.refcnt.store(0, Ordering::Release);
}

enum TaskProcessCleanupMode {
    KeepVm,
    DropVm,
}

fn cleanup_task_process_resources(
    task_ptr: *mut Task,
    resolved_id: u32,
    mode: TaskProcessCleanupMode,
) {
    unsafe {
        run_task_resource_cleanup_hooks(resolved_id);
        shm_cleanup_task(resolved_id);

        if (*task_ptr).process_id == INVALID_PROCESS_ID {
            return;
        }

        let process_id = (*task_ptr).process_id;
        let task_id = (*task_ptr).task_id;
        if !process_has_other_live_tasks(process_id, task_id) {
            fileio_destroy_table_for_process(process_id);
            if matches!(mode, TaskProcessCleanupMode::DropVm) {
                destroy_process_vm(process_id);
            }
        }
    }
}

fn process_has_other_live_tasks(process_id: u32, excluding_task_id: u32) -> bool {
    with_task_manager(|mgr| {
        for task in mgr.tasks.iter() {
            if task.task_id == excluding_task_id {
                continue;
            }
            let status = task.status();
            if status == TaskStatus::Invalid || status == TaskStatus::Terminated {
                continue;
            }
            if task.process_id == process_id {
                return true;
            }
        }
        false
    })
}

fn init_task_context(task: &mut Task) {
    task.context = TaskContext::default();
    task.fpu_state = FpuState::new();
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

    let (task, task_id) = match reserve_task_slot() {
        Ok(values) => values,
        Err(ReserveTaskSlotError::MaxTasks) => {
            klog_info!("task_create: Maximum tasks reached");
            return INVALID_TASK_ID;
        }
        Err(ReserveTaskSlotError::NoFreeSlot) => {
            klog_info!("task_create: No free task slots");
            return INVALID_TASK_ID;
        }
    };

    let resources = match allocate_task_create_resources(flags) {
        Some(resources) => resources,
        None => {
            release_task_slot(task);
            return INVALID_TASK_ID;
        }
    };

    let task_ref = unsafe { &mut *task };
    task_ref.task_id = task_id;
    unsafe { copy_name(&mut task_ref.name, name) };
    // Status stays Blocked (set by reserve_task_slot) until fully initialised.
    task_ref.priority = priority;
    task_ref.flags = flags;
    task_ref.process_id = resources.process_id;
    task_ref.tgid = task_id;
    task_ref.pgid = task_id;
    task_ref.sid = task_id;
    task_ref.controlling_tty = None;
    task_ref.clear_child_tid = 0;
    task_ref.parent_task_id = INVALID_TASK_ID;
    task_ref.stack_base = resources.stack_base;
    task_ref.stack_size = TASK_STACK_SIZE;
    task_ref.stack_pointer = resources.stack_base + TASK_STACK_SIZE - 8;
    if flags & TASK_FLAG_USER_MODE != 0 && !user_entry_is_allowed(entry_point as u64) {
        klog_info!("task_create: user entry outside user_text window");
        cleanup_task_create_resources(resources.process_id, resources.kernel_stack_base);
        release_task_slot(task);
        return INVALID_TASK_ID;
    }

    task_ref.kernel_stack_base = resources.kernel_stack_base;
    task_ref.kernel_stack_top = resources.kernel_stack_base + resources.kernel_stack_size;
    task_ref.kernel_stack_size = resources.kernel_stack_size;
    task_ref.entry_point = entry_point as usize as u64;
    task_ref.entry_arg = arg;
    task_ref.time_slice = 10;
    reset_task_runtime_fields(task_ref);
    task_ref.user_started = 0;
    task_ref.context_from_user = 0;

    init_task_context(task_ref);

    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        task_ref.context.cr3 = cpu::read_cr3() & !0xFFF;
    } else {
        let page_dir = process_vm_get_page_dir(resources.process_id);
        if !page_dir.is_null() {
            task_ref.context.cr3 = unsafe { (*page_dir).pml4_phys.as_u64() };
        }
    }

    // Transition to Ready only after context + CR3 are fully initialised.
    // reserve_task_slot() marked the slot Blocked to prevent TOCTOU races;
    // we atomically publish it as dispatchable only now.
    task_ref.set_status(TaskStatus::Ready);

    record_task_created();

    klog_debug!(
        "Created task '{}' with ID {}",
        bytes_as_str(&task_ref.name),
        task_id
    );

    task_id
}

pub fn task_terminate(task_id: u32) -> c_int {
    let (task_ptr, resolved_id) = resolve_termination_target(task_id);

    if task_id == u32::MAX && task_ptr.is_null() {
        klog_info!("task_terminate: No current task to terminate");
        return -1;
    }

    if task_ptr.is_null() || unsafe { (*task_ptr).status() } == TaskStatus::Invalid {
        klog_info!("task_terminate: Task not found");
        return -1;
    }

    if unsafe { (*task_ptr).status() } == TaskStatus::Terminated {
        return 0;
    }

    klog_info!(
        "Terminating task '{}' (ID {})",
        bytes_as_str(&unsafe { &*task_ptr }.name),
        resolved_id
    );

    let is_current = task_ptr == scheduler::scheduler_get_current_task();
    mark_task_terminated(task_ptr, resolved_id);

    if !is_current {
        cleanup_terminated_task_resources(task_ptr, resolved_id);
    } else {
        cleanup_task_process_resources(task_ptr, resolved_id, TaskProcessCleanupMode::KeepVm);
    }

    with_task_manager(|mgr| {
        if !is_current && mgr.num_tasks > 0 {
            mgr.num_tasks -= 1;
        }
        mgr.tasks_terminated = mgr.tasks_terminated.saturating_add(1);
    });

    0
}

fn resolve_termination_target(task_id: u32) -> (*mut Task, u32) {
    if task_id == u32::MAX {
        let current = scheduler::scheduler_get_current_task();
        if current.is_null() {
            (ptr::null_mut(), INVALID_TASK_ID)
        } else {
            (current, unsafe { (*current).task_id })
        }
    } else {
        (task_find_by_id(task_id), task_id)
    }
}

fn mark_task_terminated(task_ptr: *mut Task, resolved_id: u32) {
    let now = kdiag_timestamp();
    let mut should_hangup = None;
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
        (*task_ptr).set_status(TaskStatus::Terminated);
        scheduler::cancel_sleep(resolved_id);
        (*task_ptr).fate_token = 0;
        (*task_ptr).fate_value = 0;
        (*task_ptr).fate_pending = 0;
        (*task_ptr)
            .waiting_on
            .store(INVALID_TASK_ID, Ordering::Release);

        super::super::futex::futex_remove_task(task_ptr);

        let clear_tid = (*task_ptr).clear_child_tid;
        if clear_tid != 0 && task_ptr == scheduler::scheduler_get_current_task() {
            if let Ok(clear_ptr) = UserPtr::<u32>::try_new(clear_tid) {
                let _ = copy_to_user(clear_ptr, &0u32);
            }
            let _ = super::super::futex::futex_wake_one(clear_tid);
            (*task_ptr).clear_child_tid = 0;
        }

        notify_parent_of_child_exit(task_ptr);

        if (*task_ptr).sid != 0
            && (*task_ptr).task_id != INVALID_TASK_ID
            && (*task_ptr).sid == (*task_ptr).task_id
            && (*task_ptr).controlling_tty.is_some()
        {
            should_hangup = (*task_ptr).controlling_tty;
            (*task_ptr).controlling_tty = None;
        }
    }

    scheduler::unschedule_task(task_ptr);

    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    release_task_dependents(resolved_id);

    if let Some(tty_idx) = should_hangup {
        tty::hangup(tty_idx);
    }
}

fn cleanup_terminated_task_resources(task_ptr: *mut Task, resolved_id: u32) {
    cleanup_task_process_resources(task_ptr, resolved_id, TaskProcessCleanupMode::DropVm);

    unsafe {
        if (*task_ptr).ref_count() > 0 {
            defer_task_cleanup(task_ptr);
        } else {
            // Free stacks but keep the task struct in Terminated state so
            // task_find_by_id still resolves the ID.  This makes repeated
            // task_terminate() calls idempotent.  reserve_task_slot() will
            // reclaim the slot when a new task needs it.
            free_task_stacks(task_ptr);
        }
    }
}

#[inline]
fn should_collect_for_shutdown(task: &Task, task_ptr: *mut Task, current: *mut Task) -> bool {
    if task.status() == TaskStatus::Invalid {
        return false;
    }
    if task_ptr == current {
        return false;
    }
    if crate::per_cpu::is_idle_task(task_ptr) {
        return false;
    }
    task.task_id != INVALID_TASK_ID
}

fn collect_shutdown_task_ids(current: *mut Task) -> [Option<u32>; MAX_TASKS] {
    with_task_manager(|mgr| {
        let mut ids = [None; MAX_TASKS];
        for (i, task) in mgr.tasks.iter().enumerate() {
            let task_ptr = task as *const Task as *mut Task;
            if should_collect_for_shutdown(task, task_ptr, current) {
                ids[i] = Some(task.task_id);
            }
        }
        ids
    })
}

fn terminate_task_ids(task_ids: &[Option<u32>; MAX_TASKS]) -> c_int {
    let mut result = 0;
    for task_id in task_ids.iter().flatten() {
        if task_terminate(*task_id) != 0 {
            result = -1;
        }
    }
    result
}

fn refresh_num_tasks_after_shutdown() {
    with_task_manager(|mgr| {
        let mut preserved = 0u32;
        for task in mgr.tasks.iter() {
            let status = task.status();
            if status != TaskStatus::Invalid && status != TaskStatus::Terminated {
                preserved += 1;
            }
        }
        mgr.num_tasks = preserved;
    });
}

pub fn task_shutdown_all() -> c_int {
    let was_paused = crate::per_cpu::pause_all_aps();
    let current = scheduler::scheduler_get_current_task();
    let tasks_to_terminate = collect_shutdown_task_ids(current);
    let result = terminate_task_ids(&tasks_to_terminate);

    crate::per_cpu::clear_all_cpu_queues();
    refresh_num_tasks_after_shutdown();

    crate::per_cpu::resume_all_aps_if_not_nested(was_paused);
    result
}

pub fn task_fork(parent_task: *mut Task, syscall_frame: *const slopos_arch::InterruptFrame) -> u32 {
    if parent_task.is_null() {
        klog_info!("task_fork: null parent task");
        return INVALID_TASK_ID;
    }

    let parent = unsafe { &*parent_task };

    if parent.process_id == INVALID_PROCESS_ID {
        klog_info!("task_fork: parent has no process VM (kernel task?)");
        return INVALID_TASK_ID;
    }

    if parent.flags & TASK_FLAG_KERNEL_MODE != 0 {
        klog_info!("task_fork: cannot fork kernel-mode task");
        return INVALID_TASK_ID;
    }

    let child_process = match ProcessResourceLease::clone_from_parent(parent.process_id) {
        Some(process) => process,
        None => {
            klog_info!("task_fork: process_vm_clone_cow failed");
            return INVALID_TASK_ID;
        }
    };
    let child_process_id = child_process.process_id();

    let child_kernel_stack = match KernelStackLease::allocate(
        TASK_KERNEL_STACK_SIZE,
        "task_fork: failed to allocate kernel stack",
    ) {
        Some(stack) => stack,
        None => return INVALID_TASK_ID,
    };
    let child_kernel_stack_base = child_kernel_stack.base_u64();

    let (child_task_ptr, child_task_id) = match reserve_task_slot() {
        Ok(values) => values,
        Err(_) => {
            klog_info!("task_fork: no free task slots");
            return INVALID_TASK_ID;
        }
    };

    let child = unsafe { &mut *child_task_ptr };

    // SAFETY: child and parent are distinct task slots from the static TASK_TABLE,
    // and we hold exclusive access to child (just reserved).
    unsafe { child.clone_from_raw(parent) };

    child.task_id = child_task_id;
    child.process_id = child_process_id;
    child.parent_task_id = parent.task_id;
    child.tgid = child_task_id;
    child.pgid = parent.pgid;
    child.sid = parent.sid;
    child.clear_child_tid = 0;
    child.set_status(TaskStatus::Ready);

    child.kernel_stack_base = child_kernel_stack_base;
    child.kernel_stack_top = child_kernel_stack_base + TASK_KERNEL_STACK_SIZE;
    child.kernel_stack_size = TASK_KERNEL_STACK_SIZE;

    if !syscall_frame.is_null() {
        let sf = unsafe { &*syscall_frame };
        child.context.rip = sf.rip;
        child.context.rsp = sf.rsp;
        child.context.rflags = sf.rflags;
        child.context.cs = if (sf.cs & 0x3) == 0x3 { sf.cs } else { 0x23 };
        child.context.ss = if (sf.ss & 0x3) == 0x3 { sf.ss } else { 0x1B };
        child.context.ds = 0x1B;
        child.context.es = 0x1B;
        child.context.fs = 0;
        child.context.gs = 0;
        child.context.rbx = sf.rbx;
        child.context.rcx = sf.rcx;
        child.context.rdx = sf.rdx;
        child.context.rsi = sf.rsi;
        child.context.rdi = sf.rdi;
        child.context.rbp = sf.rbp;
        child.context.r8 = sf.r8;
        child.context.r9 = sf.r9;
        child.context.r10 = sf.r10;
        child.context.r11 = sf.r11;
        child.context.r12 = sf.r12;
        child.context.r13 = sf.r13;
        child.context.r14 = sf.r14;
        child.context.r15 = sf.r15;
    }
    child.context_from_user = 1;
    child.context.rax = 0;

    let child_page_dir = process_vm_get_page_dir(child_process_id);
    if !child_page_dir.is_null() {
        child.context.cr3 = unsafe { (*child_page_dir).pml4_phys.as_u64() };
    }

    reset_task_runtime_fields(child);
    let _ = child_process.disarm();
    let _ = child_kernel_stack.disarm();

    record_task_created();

    klog_debug!(
        "task_fork: created child task {} (process {}) from parent task {} (process {})",
        child_task_id,
        child_process_id,
        parent.task_id,
        parent.process_id
    );

    // Use the fork balancer (SD_BALANCE_FORK-style): spread to idlest CPU
    // instead of sticking to the parent's CPU.  Wakeups from sleep will
    // later use schedule_task() which preserves cache affinity.
    scheduler::schedule_new_task(child_task_ptr);

    child_task_id
}

pub fn task_clone(
    parent_task: *mut Task,
    flags: u64,
    child_stack: u64,
    parent_tidptr: u64,
    child_tidptr: u64,
    tls: u64,
) -> Result<u32, u64> {
    use slopos_abi::syscall::*;

    if parent_task.is_null() {
        return Err(ERRNO_EINVAL);
    }

    let parent = unsafe { &*parent_task };

    if parent.flags & TASK_FLAG_KERNEL_MODE != 0 || parent.process_id == INVALID_PROCESS_ID {
        return Err(ERRNO_EINVAL);
    }

    if flags & !CLONE_SUPPORTED_MASK != 0 {
        klog_info!("task_clone: unsupported flags 0x{:x}", flags);
        return Err(ERRNO_EINVAL);
    }

    if flags & CLONE_THREAD != 0 {
        if flags & CLONE_VM == 0 || flags & CLONE_SIGHAND == 0 {
            klog_info!("task_clone: CLONE_THREAD requires CLONE_VM | CLONE_SIGHAND");
            return Err(ERRNO_EINVAL);
        }
    }

    if flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0 {
        klog_info!("task_clone: CLONE_SIGHAND requires CLONE_VM");
        return Err(ERRNO_EINVAL);
    }

    if flags & CLONE_FILES != 0 && flags & CLONE_VM == 0 {
        klog_info!("task_clone: CLONE_FILES without CLONE_VM is unsupported");
        return Err(ERRNO_EINVAL);
    }

    let share_vm = flags & CLONE_VM != 0;
    let is_thread = flags & CLONE_THREAD != 0;

    let child_process = if share_vm {
        ProcessResourceLease::none()
    } else {
        match ProcessResourceLease::clone_from_parent(parent.process_id) {
            Some(process) => process,
            None => {
                klog_info!("task_clone: process_vm_clone_cow failed");
                return Err(ERRNO_ENOMEM);
            }
        }
    };
    let child_process_id = if share_vm {
        parent.process_id
    } else {
        child_process.process_id()
    };

    let child_kernel_stack = match KernelStackLease::allocate(
        TASK_KERNEL_STACK_SIZE,
        "task_clone: failed to allocate kernel stack",
    ) {
        Some(stack) => stack,
        None => return Err(ERRNO_ENOMEM),
    };
    let child_kernel_stack_base = child_kernel_stack.base_u64();

    let (child_task_ptr, child_task_id) = match reserve_task_slot() {
        Ok(values) => values,
        Err(_) => return Err(ERRNO_EAGAIN),
    };

    let child = unsafe { &mut *child_task_ptr };

    // SAFETY: child and parent are distinct task slots from the static TASK_TABLE,
    // and we hold exclusive access to child (just reserved).
    unsafe { child.clone_from_raw(parent) };

    child.task_id = child_task_id;
    child.process_id = child_process_id;
    child.parent_task_id = parent.task_id;

    if is_thread {
        child.tgid = if parent.tgid != INVALID_TASK_ID {
            parent.tgid
        } else {
            parent.task_id
        };
    } else {
        child.tgid = child_task_id;
    }
    child.pgid = parent.pgid;
    child.sid = parent.sid;

    child.set_status(TaskStatus::Ready);

    child.kernel_stack_base = child_kernel_stack_base;
    child.kernel_stack_top = child_kernel_stack_base + TASK_KERNEL_STACK_SIZE;
    child.kernel_stack_size = TASK_KERNEL_STACK_SIZE;

    child.context.rax = 0;

    if child_stack != 0 {
        child.context.rsp = child_stack;
    }

    if flags & CLONE_CHILD_CLEARTID != 0 && child_tidptr != 0 {
        child.clear_child_tid = child_tidptr;
    } else {
        child.clear_child_tid = 0;
    }

    if flags & CLONE_SETTLS != 0 {
        child.fs_base = tls;
    }

    if !share_vm {
        let child_page_dir = process_vm_get_page_dir(child_process_id);
        if !child_page_dir.is_null() {
            child.context.cr3 = unsafe { (*child_page_dir).pml4_phys.as_u64() };
        }
    }

    reset_task_runtime_fields(child);
    if !share_vm {
        let _ = child_process.disarm();
    }
    let _ = child_kernel_stack.disarm();
    record_task_created();

    if flags & CLONE_PARENT_SETTID != 0 && parent_tidptr != 0 {
        let parent_tid_user = match UserPtr::<u32>::try_new(parent_tidptr) {
            Ok(p) => p,
            Err(_) => {
                let _ = task_terminate(child_task_id);
                return Err(ERRNO_EFAULT);
            }
        };
        if copy_to_user(parent_tid_user, &child_task_id).is_err() {
            let _ = task_terminate(child_task_id);
            return Err(ERRNO_EFAULT);
        }
    }

    if flags & CLONE_CHILD_SETTID != 0 && child_tidptr != 0 {
        if share_vm {
            let child_tid_user = match UserPtr::<u32>::try_new(child_tidptr) {
                Ok(p) => p,
                Err(_) => {
                    let _ = task_terminate(child_task_id);
                    return Err(ERRNO_EFAULT);
                }
            };
            if copy_to_user(child_tid_user, &child_task_id).is_err() {
                let _ = task_terminate(child_task_id);
                return Err(ERRNO_EFAULT);
            }
        }
    }

    klog_info!(
        "task_clone: created child task {} (process {}, tgid {}) flags=0x{:x} from parent {} (process {})",
        child_task_id,
        child_process_id,
        child.tgid,
        flags,
        parent.task_id,
        parent.process_id
    );

    // Use the fork balancer (SD_BALANCE_FORK-style): spread to idlest CPU.
    scheduler::schedule_new_task(child_task_ptr);

    Ok(child_task_id)
}
