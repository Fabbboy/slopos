//! Bridge that registers OSTD-side wait-queue / RCU / user-mode /
//! task-runtime backends over the existing `slopos-kernel-services`
//! facades.
//!
//! OSTD does not depend on `slopos-kernel-services`. To wire the
//! production backends, this module hands OSTD function-pointer ops
//! tables (for wait-queue and RCU, whose method bodies live on the
//! kernel-services side) and registers the OSTD-internal default
//! backends for user-mode and task-runtime (whose method bodies are
//! OSTD-native and live alongside the trait definitions).

use slopos_ostd::sync::rcu::RcuOps;
use slopos_ostd::sync::wait_queue::WaitQueueOps;

use crate::driver_runtime;
use crate::platform;

/// No-op RCU log sink. The RCU stall path emits warnings via this
/// hook; without a reachable logger from this layer (utils depends on
/// kernel-services, so the inverse direction is unavailable), the
/// simplest move is a silent drop. Stall warnings are non-fatal.
fn rcu_log_warn_noop(_args: core::fmt::Arguments<'_>) {}

/// Ops table threaded into `slopos_ostd::sync::wait_queue::register_wait_queue_backend`.
/// `pub` because the boot caller in `boot::early_init::kernel_main_impl`
/// registers it inline, taking the witness from `&ctx.bsp_token()`.
pub static WAIT_QUEUE_OPS: WaitQueueOps = WaitQueueOps {
    is_runtime_initialised: driver_runtime::is_driver_runtime_initialized,
    current_task_handle: driver_runtime::current_task,
    block_current_task: driver_runtime::block_current_task,
    mark_current_blocked: driver_runtime::mark_current_blocked,
    yield_blocked_task: driver_runtime::yield_blocked_task,
    yield_blocked_task_with_timeout: driver_runtime::yield_blocked_task_with_timeout,
    unblock_task: driver_runtime::unblock_task,
    get_time_ms: platform::get_time_ms,
};

/// Ops table threaded into `slopos_ostd::sync::rcu::register_rcu_backend`.
/// `pub` for the same reason as `WAIT_QUEUE_OPS` above.
pub static RCU_OPS: RcuOps = RcuOps {
    clock_monotonic_ns: platform::clock_monotonic_ns,
    log_warn: rcu_log_warn_noop,
};
