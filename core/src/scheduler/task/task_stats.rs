use super::task_table::{task_slot_index_inner, try_with_task_manager, with_task_manager};
use super::{Task, TaskExitReason, TaskExitRecord, TaskFaultReason};

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
            let last = core::ptr::read_volatile(core::ptr::addr_of!((*from).last_run_timestamp));
            if last != 0 {
                (*from).total_runtime += timestamp.saturating_sub(last);
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

pub(super) fn record_task_created() {
    with_task_manager(|mgr| {
        mgr.num_tasks = mgr.num_tasks.saturating_add(1);
        mgr.tasks_created = mgr.tasks_created.saturating_add(1);
    });
}

pub(super) fn record_task_exit(
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
