use core::ffi::{c_int, c_void};
use core::mem;
use core::ptr;

use slopos_lib::IrqMutex;
use slopos_lib::string::bytes_as_str;
use slopos_lib::{klog_debug, klog_info};

use super::super::scheduler;
use super::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, Task, TaskExitRecord, TaskIterateCb, TaskStatus,
};
use slopos_mm::kernel_heap::kfree;

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
pub(super) fn defer_task_cleanup(task: *mut Task) {
    if task.is_null() {
        return;
    }
    ZOMBIE_LIST.lock().push(task);
}

pub(super) fn free_task_memory_and_invalidate(task: *mut Task) {
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

pub(super) struct TaskManagerInner {
    pub(super) tasks: [Task; MAX_TASKS],
    pub(super) num_tasks: u32,
    pub(super) next_task_id: u32,
    pub(super) total_context_switches: u64,
    pub(super) total_yields: u64,
    pub(super) tasks_created: u32,
    pub(super) tasks_terminated: u32,
    pub(super) exit_records: [TaskExitRecord; MAX_TASKS],
    pub(super) initialized: bool,
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

#[inline]
pub(super) fn with_task_manager<R>(f: impl FnOnce(&mut TaskManagerInner) -> R) -> R {
    let mut guard = TASK_MANAGER.lock();
    f(&mut guard)
}

#[inline]
pub(super) fn try_with_task_manager<R>(f: impl FnOnce(&mut TaskManagerInner) -> R) -> Option<R> {
    let mut guard = TASK_MANAGER.lock();
    if guard.initialized {
        Some(f(&mut guard))
    } else {
        None
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

pub(super) fn task_slot_index_inner(mgr: &TaskManagerInner, task: *const Task) -> Option<usize> {
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

pub(super) enum ReserveTaskSlotError {
    MaxTasks,
    NoFreeSlot,
}

pub(super) fn reserve_task_slot() -> Result<(*mut Task, u32), ReserveTaskSlotError> {
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
