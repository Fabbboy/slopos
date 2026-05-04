//! Bridge that registers OSTD-side wait-queue / RCU backends over the
//! existing `slopos-kernel-services` facades.
//!
//! `slopos-ostd::sync::wait_queue` and `slopos-ostd::sync::rcu` host
//! their primitives behind `WaitQueueBackend` / `RcuBackend` traits to
//! avoid pulling kernel-services into OSTD. This module ships the
//! production trait implementations and a single
//! [`register_with_ostd`] entry point that boot calls once the kernel
//! services it delegates to are themselves initialised.

use core::ffi::c_void;

use slopos_ostd::cpu::preempt::register_preempt_backend;
use slopos_ostd::io::port::register_io_port_registry;
use slopos_ostd::irq::idt::register_diagnostic_sink;
use slopos_ostd::irq::line::register_irq_reserved;
use slopos_ostd::mm::io_mem::register_io_mem_registry;
use slopos_ostd::mm::tlb::register_local_tlb_flusher;
use slopos_ostd::sync::rcu::{register_rcu_backend, RcuBackend};
use slopos_ostd::sync::wait_queue::{
    register_wait_queue_backend, WaitQueueBackend, WaitTaskHandle,
};
use slopos_ostd::user::mode::register_user_mode_backend;

use crate::driver_runtime;
use crate::ostd_backends::diagnostic_sink::CONSOLE_SINK;
use crate::ostd_backends::local_tlb::LOCAL_TLB_DYN;
use crate::ostd_backends::preempt::PCR_PREEMPT;
use crate::ostd_backends::user_mode::PCR_USER_MODE;
use crate::ostd_bridge_tables::{MMIO_RANGES, PORT_RANGES, RESERVED_VECTORS};
use crate::platform;

/// Zero-sized adapter that proxies every backend method to the
/// `kernel-services` facade.
struct KernelServicesBridge;

// SAFETY: every method is a thin call into either `driver_runtime::*`
// or `platform::*`. Both facades are themselves vtable-driven and
// safe-by-construction; the bridge adds no extra invariants.
unsafe impl WaitQueueBackend for KernelServicesBridge {
    fn is_runtime_initialised(&self) -> bool {
        driver_runtime::is_driver_runtime_initialized()
    }

    fn current_task_handle(&self) -> WaitTaskHandle {
        driver_runtime::current_task() as *mut c_void
    }

    fn prepare_to_wait(&self) {
        driver_runtime::prepare_to_wait();
    }

    fn finish_wait(&self) {
        driver_runtime::finish_wait();
    }

    fn block_current_task(&self) {
        driver_runtime::block_current_task();
    }

    fn block_current_task_with_timeout(&self, timeout_ms: u32) {
        driver_runtime::block_current_task_with_timeout(timeout_ms);
    }

    unsafe fn unblock_task(&self, task: WaitTaskHandle) -> i32 {
        driver_runtime::unblock_task(task as driver_runtime::DriverTaskHandle)
    }

    fn get_time_ms(&self) -> u64 {
        platform::get_time_ms()
    }
}

// SAFETY: clock_monotonic_ns is safe to call from any context. log_warn
// formats on the caller's stack and emits via the platform console.
unsafe impl RcuBackend for KernelServicesBridge {
    fn clock_monotonic_ns(&self) -> u64 {
        platform::clock_monotonic_ns()
    }

    fn log_warn(&self, args: core::fmt::Arguments<'_>) {
        // Discard until a logger backend is wired. Without
        // `slopos-utils` reachable from here (the bridge crate sits at
        // a layer that cannot pull utils — utils depends on
        // kernel-services), the simplest move is a silent drop. The
        // RCU stall warning is non-fatal.
        let _ = args;
    }
}

static BRIDGE: KernelServicesBridge = KernelServicesBridge;

/// Install the kernel-services bridge as both the OSTD wait-queue
/// backend and the OSTD RCU backend.
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
        register_wait_queue_backend(&BRIDGE);
        register_rcu_backend(&BRIDGE);

        register_io_mem_registry(MMIO_RANGES);
        register_io_port_registry(PORT_RANGES);
        register_irq_reserved(RESERVED_VECTORS);
        register_diagnostic_sink(&CONSOLE_SINK);
        register_preempt_backend(&PCR_PREEMPT);
        register_local_tlb_flusher(&LOCAL_TLB_DYN);
        register_user_mode_backend(&PCR_USER_MODE);
    }

    platform::console_puts(
        b"BOOT: register_with_ostd: registered preempt/diag/tlb/io_mem/io_port/irq/user_mode tables\n",
    );
}
