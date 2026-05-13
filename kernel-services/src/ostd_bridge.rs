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

use slopos_ostd::cpu::preempt::register_preempt_backend;
use slopos_ostd::io::port::register_io_port_registry;
use slopos_ostd::irq::idt::register_diagnostic_sink;
use slopos_ostd::irq::line::register_irq_reserved;
use slopos_ostd::mm::io_mem::register_io_mem_registry;
use slopos_ostd::mm::tlb::register_local_tlb_flusher;
use slopos_ostd::sync::rcu::{register_rcu_backend, RcuOps};
use slopos_ostd::sync::wait_queue::{register_wait_queue_backend, WaitQueueOps, WaitTaskHandle};
use slopos_ostd::task::register_task_runtime_backend;
use slopos_ostd::user::mode::register_user_mode_backend;

use crate::driver_runtime;
use crate::ostd_backends::diagnostic_sink::CONSOLE_SINK;
use crate::ostd_backends::local_tlb::LOCAL_TLB_DYN;
use crate::ostd_backends::preempt::PCR_PREEMPT;
use crate::ostd_bridge_tables::{MMIO_RANGES, PORT_RANGES, RESERVED_VECTORS};
use crate::platform;

/// No-op RCU log sink. The RCU stall path emits warnings via this
/// hook; without a reachable logger from this layer (utils depends on
/// kernel-services, so the inverse direction is unavailable), the
/// simplest move is a silent drop. Stall warnings are non-fatal.
fn rcu_log_warn_noop(_args: core::fmt::Arguments<'_>) {}

static WAIT_QUEUE_OPS: WaitQueueOps = WaitQueueOps {
    is_runtime_initialised: driver_runtime::is_driver_runtime_initialized,
    current_task_handle: driver_runtime::current_task,
    block_current_task: driver_runtime::block_current_task,
    mark_current_blocked: driver_runtime::mark_current_blocked,
    yield_blocked_task: driver_runtime::yield_blocked_task,
    yield_blocked_task_with_timeout: driver_runtime::yield_blocked_task_with_timeout,
    unblock_task: driver_runtime::unblock_task as unsafe fn(WaitTaskHandle) -> i32,
    get_time_ms: platform::get_time_ms,
};

static RCU_OPS: RcuOps = RcuOps {
    clock_monotonic_ns: platform::clock_monotonic_ns,
    log_warn: rcu_log_warn_noop,
};

/// Install the kernel-services bridge as the production backend for
/// each OSTD subsystem that needs a kernel-side hook.
///
/// Must be called exactly once at boot, after both
/// `register_driver_runtime_services` and
/// `register_platform_services` have been called.
///
/// # Safety
///
/// Caller must ensure the underlying kernel-services vtables are
/// populated before this is invoked; otherwise the OSTD waitqueue /
/// RCU paths will indirect through uninitialised slots.
pub unsafe fn register_with_ostd() {
    // SAFETY: every static referenced below has `'static` lifetime;
    // each OSTD register hook is one-shot and asserts on double-call.
    // `current_pcr()` is callable from this point because the BSP's
    // PCR has been installed before this function is invoked (see
    // `kernel_main_impl` in `boot/src/early_init.rs`).
    unsafe {
        register_wait_queue_backend(&WAIT_QUEUE_OPS);
        register_rcu_backend(&RCU_OPS);

        register_io_mem_registry(MMIO_RANGES);
        register_io_port_registry(PORT_RANGES);
        register_irq_reserved(RESERVED_VECTORS);
        register_diagnostic_sink(&CONSOLE_SINK);
        register_preempt_backend(&PCR_PREEMPT);
        register_local_tlb_flusher(&LOCAL_TLB_DYN);
        register_user_mode_backend(&slopos_ostd::user::mode::DEFAULT_USER_MODE_BACKEND);
        register_task_runtime_backend(&slopos_ostd::task::DEFAULT_TASK_RUNTIME_BACKEND);
    }

    platform::console_puts(
        b"BOOT: register_with_ostd: registered preempt/diag/tlb/io_mem/io_port/irq/user_mode/task_runtime tables\n",
    );
}
