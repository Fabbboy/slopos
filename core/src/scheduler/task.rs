use core::ffi::{c_char, c_int, c_void};
use core::mem;
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_abi::syscall::TtyIndex;
use slopos_lib::IrqMutex;

use slopos_lib::cpu;
use slopos_lib::kdiag_timestamp;
use slopos_lib::string::bytes_as_str;
use slopos_lib::{klog_debug, klog_info};

// =============================================================================
// Zombie List for Deferred Task Reclamation
// =============================================================================

/// List of terminated tasks waiting to be freed when refcount hits zero.
/// Protected by IrqMutex for interrupt safety.
static ZOMBIE_LIST: slopos_lib::IrqMutex<ZombieList> = slopos_lib::IrqMutex::new(ZombieList::new());

struct ZombieList {
    tasks: [Option<*mut Task>; MAX_TASKS],
    count: usize,
}

// SAFETY: ZombieList contains raw pointers to Task slots which are static.
// All access is serialized through the IrqMutex.
unsafe impl Send for ZombieList {}
unsafe impl Sync for ZombieList {}

impl ZombieList {
    const fn new() -> Self {
        Self {
            tasks: [None; MAX_TASKS],
            count: 0,
        }
    }

    fn push(&mut self, task: *mut Task) {
        if self.count < MAX_TASKS {
            self.tasks[self.count] = Some(task);
            self.count += 1;
        }
    }
}

/// Add a terminated task to the zombie list for deferred cleanup.
/// The task will be freed when its reference count reaches zero.
fn defer_task_cleanup(task: *mut Task) {
    if task.is_null() {
        return;
    }
    ZOMBIE_LIST.lock().push(task);
}

fn free_task_memory_and_invalidate(task: *mut Task) {
    if task.is_null() {
        return;
    }

    unsafe {
        let kstack = (*task).kernel_stack_base;
        let ustack = (*task).stack_base;

        if kstack != 0 {
            kfree(kstack as *mut c_void);
            (*task).kernel_stack_base = 0;
        }

        if (*task).process_id == INVALID_PROCESS_ID && ustack != 0 && ustack != kstack {
            kfree(ustack as *mut c_void);
            (*task).stack_base = 0;
        }

        *task = Task::invalid();
    }
}

/// Reap zombie tasks that are ready to be freed.
/// Should be called periodically (e.g., from scheduler idle path).
pub fn reap_zombies() {
    let mut list = ZOMBIE_LIST.lock();

    let mut write_idx = 0usize;
    for read_idx in 0..list.count {
        if let Some(task) = list.tasks[read_idx] {
            // Check if task is ready to be freed (refcount == 0)
            let ref_count = unsafe { (*task).ref_count() };
            if ref_count == 0 {
                // Safe to free now
                unsafe {
                    klog_debug!("reap_zombies: Freeing zombie task {}", (*task).task_id);
                }
                free_task_memory_and_invalidate(task);
                // Don't keep this one in the list
                list.tasks[read_idx] = None;
            } else {
                // Still has references - keep in list
                if write_idx != read_idx {
                    list.tasks[write_idx] = list.tasks[read_idx];
                    list.tasks[read_idx] = None;
                }
                write_idx += 1;
            }
        }
    }
    list.count = write_idx;
}

use super::scheduler;

pub use super::task_struct::{FpuState, Task, TaskContext};
use slopos_abi::signal::{SIGCHLD, sig_bit};
pub use slopos_abi::task::{
    BlockReason, INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, TASK_FLAG_COMPOSITOR,
    TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM,
    TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN, TASK_PRIORITY_HIGH,
    TASK_PRIORITY_IDLE, TASK_PRIORITY_LOW, TASK_PRIORITY_NORMAL, TASK_STACK_SIZE, TaskExitReason,
    TaskExitRecord, TaskFaultReason, TaskStatus,
};
pub use slopos_lib::arch::idt::IdtEntry;

use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;

pub type TaskIterateCb = Option<fn(*mut Task, *mut c_void)>;
pub type TaskEntry = fn(*mut c_void);
// =============================================================================
// Task Resource Cleanup Hooks
// =============================================================================
//
// General-purpose hook registry for cleaning up task-bound resources.
// Subsystems (video, input, etc.) register cleanup functions at init time.
// Hooks are invoked during both task termination and exec() so that the old
// process image's resources (compositor surfaces, input queues, …) are released
// before the new image starts or the task slot is reclaimed.
//
// The registry lives in `core` (low in the crate DAG).  Higher crates register
// callbacks via `register_task_resource_cleanup_hook` during their own init.

const MAX_TASK_RESOURCE_HOOKS: usize = 8;

struct TaskResourceCleanupHooks {
    hooks: [Option<fn(u32)>; MAX_TASK_RESOURCE_HOOKS],
    count: usize,
}

impl TaskResourceCleanupHooks {
    const fn new() -> Self {
        Self {
            hooks: [None; MAX_TASK_RESOURCE_HOOKS],
            count: 0,
        }
    }
}

static TASK_RESOURCE_HOOKS: IrqMutex<TaskResourceCleanupHooks> =
    IrqMutex::new(TaskResourceCleanupHooks::new());

/// Register a cleanup hook called whenever task-bound resources must be released.
///
/// This fires during:
/// - **exec()**: before the new process image starts (old window, input queue, etc. are torn down)
/// - **task termination**: as part of normal task teardown
///
/// Hooks receive the `task_id` of the task whose resources should be cleaned up.
/// Registration is expected during subsystem init and is append-only.
pub fn register_task_resource_cleanup_hook(hook: fn(u32)) {
    let mut reg = TASK_RESOURCE_HOOKS.lock();
    if reg.count >= MAX_TASK_RESOURCE_HOOKS {
        klog_info!("task_resource_hooks: registry full, hook not registered");
        return;
    }
    let idx = reg.count;
    reg.hooks[idx] = Some(hook);
    reg.count += 1;
}

/// Run all registered task resource cleanup hooks for the given task.
fn run_task_resource_cleanup_hooks(task_id: u32) {
    let hooks = TASK_RESOURCE_HOOKS.lock();
    for hook in hooks.hooks.iter().take(hooks.count) {
        if let Some(f) = hook {
            f(task_id);
        }
    }
}

/// Clean up task-bound resources before exec() replaces the process image.
///
/// Analogous to `fileio_close_on_exec` for file descriptors: tears down
/// compositor surfaces, shared-memory buffers, input queues, and any other
/// resources registered via [`register_task_resource_cleanup_hook`].
///
/// Called from `syscall_exec` after `do_exec` succeeds (point of no return).
pub fn task_cleanup_for_exec(task_id: u32) {
    run_task_resource_cleanup_hooks(task_id);
    shm_cleanup_task(task_id);
}

struct TaskManagerInner {
    tasks: [Task; MAX_TASKS],
    num_tasks: u32,
    next_task_id: u32,
    total_context_switches: u64,
    total_yields: u64,
    tasks_created: u32,
    tasks_terminated: u32,
    exit_records: [TaskExitRecord; MAX_TASKS],
    initialized: bool,
}

// SAFETY: TaskManagerInner contains Task structs with raw pointers.
// All access is serialized through the mutex.
unsafe impl Send for TaskManagerInner {}

impl TaskManagerInner {
    const fn new() -> Self {
        Self {
            tasks: [const { Task::invalid() }; MAX_TASKS],
            num_tasks: 0,
            next_task_id: 1,
            total_context_switches: 0,
            total_yields: 0,
            tasks_created: 0,
            tasks_terminated: 0,
            exit_records: [TaskExitRecord::empty(); MAX_TASKS],
            initialized: false,
        }
    }
}

static TASK_MANAGER: IrqMutex<TaskManagerInner> = IrqMutex::new(TaskManagerInner::new());

use slopos_fs::fileio::{
    fileio_clone_table_for_process, fileio_create_table_for_process,
    fileio_destroy_table_for_process,
};
use slopos_lib::kernel_services::syscall_services::tty;
use slopos_mm::kernel_heap::{kfree, kmalloc};
use slopos_mm::process_vm::{
    create_process_vm, destroy_process_vm, process_vm_clone_cow, process_vm_get_page_dir,
    process_vm_get_stack_top,
};
use slopos_mm::shared_memory::shm_cleanup_task;
use slopos_mm::user_copy::copy_to_user;
use slopos_mm::user_ptr::UserPtr;

#[inline]
fn with_task_manager<R>(f: impl FnOnce(&mut TaskManagerInner) -> R) -> R {
    let mut guard = TASK_MANAGER.lock();
    f(&mut guard)
}

#[inline]
fn try_with_task_manager<R>(f: impl FnOnce(&mut TaskManagerInner) -> R) -> Option<R> {
    let mut guard = TASK_MANAGER.lock();
    if guard.initialized {
        Some(f(&mut guard))
    } else {
        None
    }
}

pub fn task_find_by_id(task_id: u32) -> *mut Task {
    // INVALID_TASK_ID is used for uninitialized/invalid task slots - never return those
    if task_id == INVALID_TASK_ID {
        return ptr::null_mut();
    }

    with_task_manager(|mgr| {
        for task in mgr.tasks.iter_mut() {
            if task.task_id == task_id {
                return task as *mut Task;
            }
        }
        ptr::null_mut()
    })
}

/// Find a live task whose active address space matches `cr3`.
///
/// This is primarily used by exception paths that need to recover the faulting
/// task even if per-CPU scheduler current-task metadata is temporarily stale.
pub fn task_find_by_cr3(cr3: u64) -> *mut Task {
    if cr3 == 0 {
        return ptr::null_mut();
    }

    let target = cr3 & !0xFFF;

    with_task_manager(|mgr| {
        let mut fallback: *mut Task = ptr::null_mut();

        for task in mgr.tasks.iter_mut() {
            let status = task.status();
            if status == TaskStatus::Invalid || status == TaskStatus::Terminated {
                continue;
            }

            let task_cr3 =
                unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(task.context.cr3)) }
                    & !0xFFF;
            if task_cr3 != target {
                continue;
            }

            let task_ptr = task as *mut Task;
            if status == TaskStatus::Running {
                return task_ptr;
            }

            if fallback.is_null() {
                fallback = task_ptr;
            }
        }

        fallback
    })
}

fn release_task_dependents(completed_task_id: u32) {
    // Collect task pointers first, then try to wake outside the lock
    // This prevents holding the task manager lock during wake operations
    let candidates: [Option<*mut Task>; MAX_TASKS] = with_task_manager(|mgr| {
        let mut result = [None; MAX_TASKS];
        let mut idx = 0;
        for dependent in mgr.tasks.iter_mut() {
            // Skip non-blocked tasks
            if !task_is_blocked(dependent) {
                continue;
            }
            // Check if waiting on the completed task (atomic load)
            if dependent.waiting_on.load(Ordering::Acquire) != completed_task_id {
                continue;
            }
            // Candidate for wakeup - collect pointer
            result[idx] = Some(dependent as *mut Task);
            idx += 1;
        }
        result
    });

    // Now attempt to wake each candidate using the single-winner protocol
    // CAS ensures exactly one waker wins per task
    for candidate_opt in candidates.iter() {
        if let Some(task_ptr) = candidate_opt {
            let task = *task_ptr;
            let task_id = unsafe { (*task).task_id };

            if scheduler::try_wake_from_task_wait(task, completed_task_id) {
                klog_info!(
                    "release_task_dependents: Woke task {} (was waiting on {})",
                    task_id,
                    completed_task_id
                );
            }
            // If try_wake returns false, another waker won or task changed state
            // Either way, nothing more for us to do
        }
    }
}

fn user_entry_is_allowed(addr: u64) -> bool {
    // User tasks must start from filesystem-loaded ELF code in PROCESS_CODE_START_VA.
    const PROCESS_CODE_END: u64 = 0x0000_0000_0050_0000; // 1MB range
    addr >= PROCESS_CODE_START_VA && addr < PROCESS_CODE_END
}

struct TaskCreateResources {
    process_id: u32,
    stack_base: u64,
    kernel_stack_base: u64,
    kernel_stack_size: u64,
}

/// Owns process-scoped resources during task setup and tears them down on failure.
///
/// This keeps partial setup paths correct without open-coded cleanup in each caller.
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

    /// Transfer ownership to the task lifecycle.
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

/// Owns a kernel stack allocation during setup and frees it unless transferred.
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

fn task_slot_index_inner(mgr: &TaskManagerInner, task: *const Task) -> Option<usize> {
    if task.is_null() {
        return None;
    }
    let start = mgr.tasks.as_ptr() as usize;
    let task_size = mem::size_of::<Task>();
    let total_span = task_size.saturating_mul(MAX_TASKS);
    let end = start.saturating_add(total_span);
    let addr = task as usize;

    if addr < start || addr >= end {
        return None;
    }

    let rel = addr - start;
    if rel % task_size != 0 {
        return None;
    }

    Some(rel / task_size)
}

pub fn task_pointer_is_valid(task: *const Task) -> bool {
    with_task_manager(|mgr| task_slot_index_inner(mgr, task).is_some())
}

enum ReserveTaskSlotError {
    MaxTasks,
    NoFreeSlot,
}

fn reserve_task_slot() -> Result<(*mut Task, u32), ReserveTaskSlotError> {
    with_task_manager(|mgr| {
        if mgr.num_tasks >= MAX_TASKS as u32 {
            return Err(ReserveTaskSlotError::MaxTasks);
        }

        let mut slot: *mut Task = ptr::null_mut();
        for task in mgr.tasks.iter_mut() {
            if task.status() == TaskStatus::Invalid {
                slot = task as *mut Task;
                break;
            }
        }

        if slot.is_null() {
            return Err(ReserveTaskSlotError::NoFreeSlot);
        }

        if let Some(idx) = task_slot_index_inner(mgr, slot) {
            mgr.exit_records[idx] = TaskExitRecord::empty();
        }

        let task_id = mgr.next_task_id;
        mgr.next_task_id = task_id.wrapping_add(1);

        Ok((slot, task_id))
    })
}

fn record_task_created() {
    with_task_manager(|mgr| {
        mgr.num_tasks = mgr.num_tasks.saturating_add(1);
        mgr.tasks_created = mgr.tasks_created.saturating_add(1);
    });
}

fn record_task_exit(
    task: *const Task,
    exit_reason: TaskExitReason,
    fault_reason: TaskFaultReason,
    exit_code: u32,
) {
    with_task_manager(|mgr| {
        if let Some(idx) = task_slot_index_inner(mgr, task) {
            mgr.exit_records[idx] = TaskExitRecord {
                task_id: unsafe { (*task).task_id },
                exit_reason,
                fault_reason,
                exit_code,
            };
        }
    });
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

fn notify_parent_of_child_exit(task_ptr: *mut Task) {
    if task_ptr.is_null() {
        return;
    }

    let (task_id, tgid, parent_task_id) = unsafe {
        (
            (*task_ptr).task_id,
            (*task_ptr).tgid,
            (*task_ptr).parent_task_id,
        )
    };

    if parent_task_id == INVALID_TASK_ID || parent_task_id == task_id {
        return;
    }

    if tgid != task_id {
        return;
    }

    let parent = task_find_by_id(parent_task_id);
    if parent.is_null() {
        return;
    }

    unsafe {
        (*parent)
            .signal_pending
            .fetch_or(sig_bit(SIGCHLD), Ordering::AcqRel);
    }
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
pub fn init_task_manager() -> c_int {
    with_task_manager(|mgr| {
        mgr.total_context_switches = 0;
        mgr.total_yields = 0;
        mgr.tasks_created = 0;
        mgr.tasks_terminated = 0;

        let mut preserved_count = 0u32;
        let mut max_task_id = 0u32;
        for task in mgr.tasks.iter_mut() {
            let task_ptr = task as *mut Task;
            if crate::per_cpu::is_idle_task(task_ptr) {
                preserved_count += 1;
                if task.task_id > max_task_id {
                    max_task_id = task.task_id;
                }
                klog_debug!(
                    "init_task_manager: preserving idle task {} ('{}')",
                    task.task_id,
                    bytes_as_str(&task.name)
                );
                continue;
            }
            *task = Task::invalid();
        }
        for rec in mgr.exit_records.iter_mut() {
            *rec = TaskExitRecord::empty();
        }
        mgr.num_tasks = preserved_count;
        mgr.next_task_id = max_task_id + 1;
        mgr.initialized = true;
    });
    // Invariants restored -- clear any poisoned state from panic recovery.
    TASK_MANAGER.clear_poison();
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
        None => return INVALID_TASK_ID,
    };

    let task_ref = unsafe { &mut *task };
    task_ref.task_id = task_id;
    unsafe { copy_name(&mut task_ref.name, name) };
    task_ref.set_status(TaskStatus::Ready);
    task_ref.priority = priority;
    task_ref.flags = flags;
    task_ref.process_id = resources.process_id;
    // New process: task is its own thread-group leader.
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
        *task_ref = Task::invalid();
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

        // Do not free/defer the current task structure here: we are still
        // executing on its kernel stack until syscall_exit switches away.
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

        super::futex::futex_remove_task(task_ptr);

        let clear_tid = (*task_ptr).clear_child_tid;
        if clear_tid != 0 && task_ptr == scheduler::scheduler_get_current_task() {
            if let Ok(clear_ptr) = UserPtr::<u32>::try_new(clear_tid) {
                let _ = copy_to_user(clear_ptr, &0u32);
            }
            let _ = super::futex::futex_wake_one(clear_tid);
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
            free_task_memory_and_invalidate(task_ptr);
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

pub fn task_get_info(task_id: u32, task_info: *mut *mut Task) -> c_int {
    if task_info.is_null() {
        return -1;
    }
    let task = task_find_by_id(task_id);
    unsafe {
        if task.is_null() || (*task).status() == TaskStatus::Invalid {
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
    with_task_manager(|mgr| {
        for rec in mgr.exit_records.iter() {
            if rec.task_id == task_id {
                unsafe { *record_out = *rec };
                return 0;
            }
        }
        -1
    })
}

pub fn task_set_state(task_id: u32, new_status: TaskStatus) -> c_int {
    let task = task_find_by_id(task_id);
    if task.is_null() {
        return -1;
    }

    let task_ref = unsafe { &*task };
    if task_ref.status() == TaskStatus::Invalid {
        return -1;
    }

    transition_to_c_int(task_ref.try_transition_to(new_status))
}

#[inline]
fn transition_to_c_int(success: bool) -> c_int {
    if success { 0 } else { -1 }
}

fn apply_state_transition(
    task_ref: &mut Task,
    new_status: TaskStatus,
    reason: BlockReason,
) -> c_int {
    match new_status {
        TaskStatus::Ready => transition_to_c_int(task_ref.mark_ready()),
        TaskStatus::Running => transition_to_c_int(task_ref.mark_running()),
        TaskStatus::WillBlock => {
            transition_to_c_int(task_ref.try_transition_to(TaskStatus::WillBlock))
        }
        TaskStatus::Blocked => transition_to_c_int(task_ref.block(reason)),
        TaskStatus::Terminated => transition_to_c_int(task_ref.terminate()),
        TaskStatus::Invalid => -1,
    }
}

pub fn task_set_state_with_reason(
    task_id: u32,
    new_status: TaskStatus,
    reason: BlockReason,
) -> c_int {
    let task = task_find_by_id(task_id);
    if task.is_null() {
        return -1;
    }

    let task_ref = unsafe { &mut *task };
    if task_ref.status() == TaskStatus::Invalid {
        return -1;
    }

    apply_state_transition(task_ref, new_status, reason)
}
pub fn get_task_stats(total_tasks: *mut u32, active_tasks: *mut u32, context_switches: *mut u64) {
    with_task_manager(|mgr| {
        if !total_tasks.is_null() {
            unsafe { *total_tasks = mgr.tasks_created };
        }
        if !active_tasks.is_null() {
            unsafe { *active_tasks = mgr.num_tasks };
        }
        if !context_switches.is_null() {
            unsafe { *context_switches = mgr.total_context_switches };
        }
    });
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
        with_task_manager(|mgr| {
            mgr.total_context_switches += 1;
        });
    }
}

pub fn task_record_yield(task: *mut Task) {
    with_task_manager(|mgr| {
        mgr.total_yields += 1;
    });
    if !task.is_null() {
        unsafe { (*task).yield_count = (*task).yield_count.saturating_add(1) };
    }
}

pub fn task_get_total_yields() -> u64 {
    try_with_task_manager(|mgr| mgr.total_yields).unwrap_or(0)
}

pub fn task_state_to_string(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Invalid => "invalid",
        TaskStatus::Ready => "ready",
        TaskStatus::Running => "running",
        TaskStatus::WillBlock => "willblock",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Terminated => "terminated",
    }
}

pub fn task_iterate_active(callback: TaskIterateCb, context: *mut c_void) {
    let cb = match callback {
        Some(cb) => cb,
        None => return,
    };

    let task_ptrs = with_task_manager(|mgr| {
        let mut ptrs = [None; MAX_TASKS];
        for (i, task) in mgr.tasks.iter_mut().enumerate() {
            if task.status() != TaskStatus::Invalid && task.task_id != INVALID_TASK_ID {
                ptrs[i] = Some(task as *mut Task);
            }
        }
        ptrs
    });

    for task in task_ptrs.iter().flatten() {
        cb(*task, context);
    }
}

struct ClearControllingTtyContext {
    session_id: u32,
    tty: TtyIndex,
    cleared: usize,
}

fn clear_controlling_tty_for_session_task(task: *mut Task, context: *mut c_void) {
    if task.is_null() || context.is_null() {
        return;
    }

    let ctx = unsafe { &mut *context.cast::<ClearControllingTtyContext>() };
    unsafe {
        if (*task).sid == ctx.session_id && (*task).controlling_tty == Some(ctx.tty) {
            (*task).controlling_tty = None;
            ctx.cleared = ctx.cleared.saturating_add(1);
        }
    }
}

pub fn task_clear_controlling_tty_for_session(session_id: u32, tty: TtyIndex) -> usize {
    if session_id == 0 {
        return 0;
    }

    let mut ctx = ClearControllingTtyContext {
        session_id,
        tty,
        cleared: 0,
    };
    task_iterate_active(
        Some(clear_controlling_tty_for_session_task),
        (&mut ctx as *mut ClearControllingTtyContext).cast(),
    );
    ctx.cleared
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
        let current_status = (*task).status();
        if current_status != TaskStatus::Ready && current_status != TaskStatus::Running {
            klog_info!(
                "task_set_current: unexpected state {} for task {} ('{}')",
                current_status.as_u8() as u32,
                (*task).task_id,
                bytes_as_str(&(*task).name)
            );
        }
        (*task).set_status(TaskStatus::Running);
    }
}
pub fn task_get_state(task: *const Task) -> TaskStatus {
    if task.is_null() {
        return TaskStatus::Invalid;
    }
    unsafe { (*task).status() }
}
pub fn task_is_ready(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::Ready
}
pub fn task_is_running(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::Running
}
pub fn task_is_will_block(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::WillBlock
}
pub fn task_is_blocked(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::Blocked
}
pub fn task_is_terminated(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::Terminated
}
pub fn task_is_invalid(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::Invalid
}

/// Fork the current task, creating a new child process.
///
/// `syscall_frame` is the InterruptFrame from the SYSCALL/INT 0x80 entry.
/// The child's context is built from this frame (user-mode RIP, RSP, RFLAGS,
/// etc.) so that the child always returns to user mode — even if the parent
/// was preempted by a timer tick (which would overwrite the Task's `context`
/// field with a kernel-mode snapshot).
pub fn task_fork(parent_task: *mut Task, syscall_frame: *const slopos_lib::InterruptFrame) -> u32 {
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
    // Fork creates a new process: child is its own thread-group leader.
    child.tgid = child_task_id;
    child.pgid = parent.pgid;
    child.sid = parent.sid;
    child.clear_child_tid = 0;
    child.set_status(TaskStatus::Ready);

    child.kernel_stack_base = child_kernel_stack_base;
    child.kernel_stack_top = child_kernel_stack_base + TASK_KERNEL_STACK_SIZE;
    child.kernel_stack_size = TASK_KERNEL_STACK_SIZE;

    // Build the child's context from the SYSCALL entry frame, NOT from
    // the parent's Task.context (which may reflect a timer-tick preemption
    // with CS=0x08 / kernel RIP).  The child must return to user mode as
    // if fork() just returned 0.
    if !syscall_frame.is_null() {
        let sf = unsafe { &*syscall_frame };
        child.context.rip = sf.rip;
        child.context.rsp = sf.rsp;
        child.context.rflags = sf.rflags;
        // Fork always returns to user mode. Force user segments even if the
        // parent's saved Task.context currently contains kernel selectors.
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
    // Mark as returning from user mode so the scheduler uses IRETQ.
    child.context_from_user = 1;
    child.context.rax = 0; // fork() returns 0 in child

    let child_page_dir = process_vm_get_page_dir(child_process_id);
    if !child_page_dir.is_null() {
        child.context.cr3 = unsafe { (*child_page_dir).pml4_phys.as_u64() };
    }

    reset_task_runtime_fields(child);
    // Ownership is now carried by the child task lifecycle.
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

    // Enqueue the child in the scheduler's per-CPU ready queue so it actually runs.
    scheduler::schedule_task(child_task_ptr);

    child_task_id
}

/// Create a new task via clone(2) semantics.
///
/// `flags`: Combination of CLONE_VM, CLONE_FILES, CLONE_THREAD, etc.
/// `child_stack`: If non-zero, the child's initial RSP. If zero, inherit parent RSP (fork-like).
/// `parent_tidptr`: User address to write the child's TID (if CLONE_PARENT_SETTID).
/// `child_tidptr`: User address for CLONE_CHILD_SETTID / CLONE_CHILD_CLEARTID.
/// `tls`: New FS_BASE for child (if CLONE_SETTLS).
///
/// Returns `Ok(child_task_id)` on success or `Err(errno_u64)` on failure.
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

    // Reject kernel-mode tasks and tasks without a process VM.
    if parent.flags & TASK_FLAG_KERNEL_MODE != 0 || parent.process_id == INVALID_PROCESS_ID {
        return Err(ERRNO_EINVAL);
    }

    // Reject unsupported flag combinations.
    if flags & !CLONE_SUPPORTED_MASK != 0 {
        klog_info!("task_clone: unsupported flags 0x{:x}", flags);
        return Err(ERRNO_EINVAL);
    }

    // CLONE_THREAD requires CLONE_VM and CLONE_SIGHAND (Linux semantics).
    if flags & CLONE_THREAD != 0 {
        if flags & CLONE_VM == 0 || flags & CLONE_SIGHAND == 0 {
            klog_info!("task_clone: CLONE_THREAD requires CLONE_VM | CLONE_SIGHAND");
            return Err(ERRNO_EINVAL);
        }
    }

    // CLONE_SIGHAND requires CLONE_VM (Linux semantics).
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

    // Allocate a kernel stack for the child.
    let child_kernel_stack = match KernelStackLease::allocate(
        TASK_KERNEL_STACK_SIZE,
        "task_clone: failed to allocate kernel stack",
    ) {
        Some(stack) => stack,
        None => return Err(ERRNO_ENOMEM),
    };
    let child_kernel_stack_base = child_kernel_stack.base_u64();

    // Reserve a task slot for the child.
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

    // Thread-group identity.
    if is_thread {
        // Thread joins the parent's thread group.
        child.tgid = if parent.tgid != INVALID_TASK_ID {
            parent.tgid
        } else {
            parent.task_id
        };
    } else {
        // New process: child is its own thread-group leader.
        child.tgid = child_task_id;
    }
    child.pgid = parent.pgid;
    child.sid = parent.sid;

    child.set_status(TaskStatus::Ready);

    // Set up kernel stack.
    child.kernel_stack_base = child_kernel_stack_base;
    child.kernel_stack_top = child_kernel_stack_base + TASK_KERNEL_STACK_SIZE;
    child.kernel_stack_size = TASK_KERNEL_STACK_SIZE;

    // Child returns 0 from clone.
    child.context.rax = 0;

    // If a child stack was provided, use it.
    if child_stack != 0 {
        child.context.rsp = child_stack;
    }

    // CLONE_CHILD_CLEARTID: remember the address for exit-time futex wake.
    if flags & CLONE_CHILD_CLEARTID != 0 && child_tidptr != 0 {
        child.clear_child_tid = child_tidptr;
    } else {
        child.clear_child_tid = 0;
    }

    if flags & CLONE_SETTLS != 0 {
        child.fs_base = tls;
    }

    // Set CR3 for the child's address space.
    if !share_vm {
        let child_page_dir = process_vm_get_page_dir(child_process_id);
        if !child_page_dir.is_null() {
            child.context.cr3 = unsafe { (*child_page_dir).pml4_phys.as_u64() };
        }
    }
    // If sharing VM, CR3 is already inherited from clone_from_raw.

    reset_task_runtime_fields(child);
    // Ownership is now carried by the child task lifecycle.
    if !share_vm {
        let _ = child_process.disarm();
    }
    let _ = child_kernel_stack.disarm();
    record_task_created();

    // CLONE_PARENT_SETTID: write child TID into parent's memory.
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

    // CLONE_CHILD_SETTID: write child TID into child's memory at child_tidptr.
    // For CLONE_VM threads, the address space is shared so we can write directly.
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
        // For non-CLONE_VM (fork-like), we'd need to write into the child's
        // COW address space. This is complex; defer for now.
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

    // Enqueue the child in the scheduler's per-CPU ready queue so it actually runs.
    scheduler::schedule_task(child_task_ptr);

    Ok(child_task_id)
}

pub unsafe fn task_manager_force_unlock() {
    unsafe { TASK_MANAGER.force_unlock() };
}

/// Force-unlock the task manager AND mark it as poisoned.
/// Called from panic recovery to signal that the task table may be
/// in an inconsistent state. The next `init_task_manager()` call
/// clears the poison after reinitializing invariants.
pub unsafe fn task_manager_poison_unlock() {
    unsafe { TASK_MANAGER.poison_unlock() };
}

use super::ffi_boundary::task_entry_wrapper;
