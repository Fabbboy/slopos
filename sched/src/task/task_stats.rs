use slopos_ostd::task::TaskAddr;

use super::Task;
use super::task_table::{try_with_task_manager, with_task_manager};

pub fn get_task_stats(total_tasks: *mut u32, active_tasks: *mut u32, context_switches: *mut u64) {
    use slopos_ostd::util::ptr_buf::nullable_write;
    with_task_manager(|mgr| {
        nullable_write(total_tasks, mgr.tasks_created);
        nullable_write(active_tasks, mgr.num_tasks);
        nullable_write(context_switches, mgr.total_context_switches);
    });
}

pub fn task_record_context_switch(from: Option<&Task>, to: Option<&Task>, timestamp: u64) {
    if let Some(from) = from {
        let last = from.last_run_timestamp();
        if last != 0 {
            from.add_total_runtime(timestamp.saturating_sub(last));
        }
        from.set_last_run_timestamp(0);
    }

    if let Some(to) = to {
        to.set_last_run_timestamp(timestamp);
        // Task identity is an address question, not a value question, so this
        // compares `TaskAddr` rather than reaching for a `PartialEq` on the
        // task body — `TaskInner` deliberately has none. The `from == None`
        // case counts as a switch, matching the null-`from` behaviour a raw
        // pointer comparison gave.
        if from.map(TaskAddr::of) != Some(TaskAddr::of(to)) {
            with_task_manager(|mgr| {
                mgr.total_context_switches += 1;
            });
        }
    }
}

pub fn task_record_yield(task: &Task) {
    with_task_manager(|mgr| {
        mgr.total_yields += 1;
    });
    task.inc_yield_count();
}

pub fn task_get_total_yields() -> u64 {
    try_with_task_manager(|mgr| mgr.total_yields).unwrap_or(0)
}

pub(super) fn record_task_created() {
    with_task_manager(|mgr| {
        // num_tasks is already incremented by task allocation; only
        // bump the lifetime counter here.
        mgr.tasks_created = mgr.tasks_created.saturating_add(1);
    });
}
