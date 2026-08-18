//! Wait-queue and RCU backends for OSTD, built over the
//! `slopos-kernel-services` facades. OSTD does not depend on this crate, so it
//! takes function-pointer ops tables rather than a direct call edge.

use slopos_ostd::sync::rcu::RcuOps;
use slopos_ostd::sync::wait_queue::WaitQueueOps;

use crate::driver_runtime;
use crate::platform;

/// No-op sink: `slopos-utils` depends on this crate, so no logger is reachable
/// from here and the non-fatal RCU stall warnings are dropped.
fn rcu_log_warn_noop(_args: core::fmt::Arguments<'_>) {}

/// Ops table for `slopos_ostd::sync::wait_queue::register_wait_queue_backend`;
/// `pub` because the boot caller registers it inline with a BSP-token witness.
pub static WAIT_QUEUE_OPS: WaitQueueOps = WaitQueueOps {
    is_runtime_initialised: driver_runtime::is_driver_runtime_initialized,
    current_task_handle: driver_runtime::current_task_handle,
    mark_current_blocked: driver_runtime::mark_current_blocked,
    yield_blocked_task: driver_runtime::yield_blocked_task,
    yield_blocked_task_with_timeout: driver_runtime::yield_blocked_task_with_timeout,
    set_current_runnable: driver_runtime::set_current_runnable,
    unblock_task: driver_runtime::unblock_task,
    get_time_ms: platform::get_time_ms,
    swap_parked_queue: driver_runtime::swap_parked_wait_queue,
    current_task_is_killed: driver_runtime::current_task_is_killed,
    current_task_has_deliverable_signal: driver_runtime::has_pending_signal,
};

/// Ops table for `slopos_ostd::sync::rcu::register_rcu_backend`.
pub static RCU_OPS: RcuOps = RcuOps {
    clock_monotonic_ns: platform::clock_monotonic_ns,
    log_warn: rcu_log_warn_noop,
};
