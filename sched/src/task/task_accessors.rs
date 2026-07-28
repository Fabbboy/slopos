//! Re-export of the OSTD-owned task accessor functions.
//!
//! The bodies live in `slopos_ostd::task::accessors`; kernel callers spell each
//! accessor as `crate::task::task_accessors::*`.
//!
//! The list is written out rather than globbed so the re-exported surface is
//! enumerable. Each accessor that loses its last caller is then removed from one
//! named place, and the compiler reports the removal as an unresolved import
//! rather than letting a glob silently keep resolving.

pub use slopos_ostd::task::accessors::{
    TASK_EXIT_CLEANUP_ACCOUNTED, TASK_EXIT_CLEANUP_RESOURCES, TASK_EXIT_CLEANUP_VM,
    child_exit_event, signal_pending_event, task_add_total_runtime, task_borrow, task_borrow_mut,
    task_children_is_empty, task_children_pop, task_children_remove, task_clone_from,
    task_context_cs, task_context_rip, task_context_rsp, task_context_ss, task_controlling_tty,
    task_cpu_affinity, task_default_signals_in_mask, task_entry_point, task_exit_cleanup_mark,
    task_exit_info_is_set, task_exit_info_ref, task_flags, task_fs_base,
    task_has_deliverable_signal, task_has_flag, task_id_of, task_install_idle_affinity,
    task_is_blocked, task_is_exited, task_is_invalid, task_is_ready, task_is_running,
    task_is_terminated, task_kernel_stack_seed_ret, task_kernel_stack_top, task_last_cpu,
    task_last_run_timestamp, task_name_looks_idle, task_next_inbox_load,
    task_next_inbox_store_relaxed, task_next_inbox_store_release, task_on_cpu_load,
    task_panic_in_flight_load, task_panic_in_flight_store, task_parent_task_id,
    task_pcr_round_trip_swap, task_pgid, task_priority, task_process_group, task_process_id,
    task_reclaim_next_store_relaxed, task_recovery_depth_load, task_recovery_depth_store,
    task_remote_inbox_is_linked, task_remote_inbox_try_link, task_remote_inbox_unlink,
    task_reset_caught_handlers, task_reset_fpu_state, task_save_from_interrupt_frame,
    task_sched_placement_compare_exchange, task_sched_placement_load, task_sched_placement_store,
    task_session, task_set_cpu_affinity, task_set_fs_base, task_set_kernel_stack_top,
    task_set_last_run_timestamp, task_set_on_cpu, task_set_parent_task_id, task_set_status,
    task_set_time_slice, task_set_time_slice_remaining, task_sid, task_signal_blocked,
    task_signal_handler, task_signal_pending, task_signal_pending_store, task_signal_post,
    task_signal_raise, task_status, task_switch_ctx_rflags, task_switch_ctx_rip_rsp, task_tgid,
    task_time_slice, task_time_slice_remaining, task_user_ctx_ptr, task_waiter_count,
    task_wake_all_waiters,
};

use super::task_table::task_find_by_id;

/// Establish `parent_task_id` as the parent of `task_id`: sets the child's
/// parent id and parks one owning reference in the parent's children list (via
/// [`link_child`](super::link_child)). Returns 0 on success, -1 if either task
/// cannot be located. Both guards pin their task across the link.
pub fn task_set_parent(task_id: u32, parent_task_id: u32) -> core::ffi::c_int {
    let Some(child) = task_find_by_id(task_id) else {
        return -1;
    };
    let Some(parent) = task_find_by_id(parent_task_id) else {
        return -1;
    };
    let Some(child_nn) = core::ptr::NonNull::new(child.as_ptr()) else {
        return -1;
    };
    super::link_child(&parent, child_nn);
    0
}
