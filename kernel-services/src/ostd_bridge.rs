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

use slopos_ostd::sync::rcu::{register_rcu_backend, RcuBackend};
use slopos_ostd::sync::wait_queue::{
    register_wait_queue_backend, WaitQueueBackend, WaitTaskHandle,
};

use crate::driver_runtime;
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
    // SAFETY: BRIDGE is a `'static` ZST; the registration hooks
    // require `'static` lifetime which a static reference satisfies.
    unsafe {
        register_wait_queue_backend(&BRIDGE);
        register_rcu_backend(&BRIDGE);
    }
}
