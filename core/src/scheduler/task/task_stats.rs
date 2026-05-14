use super::Task;
use super::task_accessors::{
    task_add_total_runtime, task_last_run_timestamp_volatile, task_set_last_run_timestamp,
    task_yield_count_inc,
};
use super::task_table::{try_with_task_manager, with_task_manager};

pub fn get_task_stats(total_tasks: *mut u32, active_tasks: *mut u32, context_switches: *mut u64) {
    with_task_manager(|mgr| {
        // SAFETY: out-pointers are caller-owned; non-null check gates
        // each write; nothing else aliases the slot during this call.
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
        let last = task_last_run_timestamp_volatile(from).unwrap_or(0);
        if last != 0 {
            task_add_total_runtime(from, timestamp.saturating_sub(last));
        }
        task_set_last_run_timestamp(from, 0);
    }

    if !to.is_null() {
        task_set_last_run_timestamp(to, timestamp);
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
        task_yield_count_inc(task);
    }
}

pub fn task_get_total_yields() -> u64 {
    try_with_task_manager(|mgr| mgr.total_yields).unwrap_or(0)
}

pub(super) fn record_task_created() {
    with_task_manager(|mgr| {
        // num_tasks is already incremented by reserve_task_slot(); only
        // bump the lifetime counter here.
        mgr.tasks_created = mgr.tasks_created.saturating_add(1);
    });
}
