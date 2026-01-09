//! Scheduler trait interfaces - breaks circular dependencies between crates.
//!
//! These traits are defined in `abi` (no dependencies) so that:
//! - `drivers` can depend on `abi` and call through trait objects
//! - `sched` can depend on `abi` and implement the traits
//! - `boot` can depend on both and wire them together
//!
//! This replaces the 415-line unsafe callback system in scheduler_callbacks.rs

use core::ffi::{c_char, c_int, c_void};

/// Result from a Wheel of Fate spin.
/// Moved here from drivers/fate.rs to break the dependency cycle.
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct FateResult {
    pub token: u32,
    pub value: u32,
}

/// Opaque task handle for cross-crate use.
/// Using raw pointer allows passing task references without importing Task type.
pub type TaskHandle = *mut c_void;

/// Scheduler timing and interrupt handling.
/// Called from IRQ handlers in drivers.
pub trait SchedulerTiming: Send + Sync {
    /// Called on each timer tick interrupt.
    fn timer_tick(&self);

    /// Called after IRQ dispatch completes, for post-interrupt cleanup.
    fn handle_post_irq(&self);

    /// Request a reschedule from interrupt context (deferred until safe).
    fn request_reschedule_from_interrupt(&self);
}

/// Task execution control.
/// Called from syscall handlers and blocking I/O.
pub trait SchedulerExecution: Send + Sync {
    /// Get currently running task (null if none).
    fn get_current_task(&self) -> TaskHandle;

    /// Voluntarily yield CPU to scheduler.
    fn yield_cpu(&self);

    /// Invoke scheduler to pick next task.
    fn schedule(&self);

    /// Terminate a task by ID. Returns 0 on success, negative on error.
    fn task_terminate(&self, task_id: u32) -> c_int;

    /// Block the currently running task.
    fn block_current_task(&self);

    /// Check if a task is blocked.
    fn task_is_blocked(&self, task: TaskHandle) -> bool;

    /// Unblock a task. Returns 0 on success, negative on error.
    fn unblock_task(&self, task: TaskHandle) -> c_int;
}

/// Scheduler state queries.
/// Called for syscall_get_info and idle task management.
pub trait SchedulerState: Send + Sync {
    /// Check if scheduler is enabled (1 = yes, 0 = no).
    fn is_enabled(&self) -> c_int;

    /// Check if preemption is enabled (1 = yes, 0 = no).
    fn is_preemption_enabled(&self) -> c_int;

    /// Get task statistics: (total_tasks, active_tasks, context_switches).
    fn get_task_stats(&self) -> (u32, u32, u64);

    /// Get scheduler statistics: (context_switches, yields, ready_tasks, schedule_calls).
    fn get_scheduler_stats(&self) -> (u64, u64, u32, u32);

    /// Register callback for idle task wakeup condition.
    fn register_idle_wakeup_callback(&self, cb: Option<fn() -> c_int>);
}

/// Wheel of Fate integration (gambling subsystem).
/// The wizards' addiction to gambling is codified here.
pub trait SchedulerFate: Send + Sync {
    /// Spin the wheel, get a fate result.
    fn fate_spin(&self) -> FateResult;

    /// Store pending fate for a task. Returns 0 on success.
    fn fate_set_pending(&self, res: FateResult, task_id: u32) -> c_int;

    /// Retrieve and clear pending fate. Returns Some(result) if pending, None otherwise.
    fn fate_take_pending(&self, task_id: u32) -> Option<FateResult>;

    /// Apply outcome (award W or L currency).
    fn fate_apply_outcome(&self, res: &FateResult, resolution: u32, award: bool);
}

/// Boot/kernel services - separate from scheduler.
/// Provides access to boot-time data and critical kernel operations.
///
/// Note: HHDM access (get_hhdm_offset, is_hhdm_available) has been moved to
/// slopos_mm::hhdm module for direct access without trait indirection.
pub trait BootServices: Send + Sync {
    /// Check if ACPI RSDP is available.
    fn is_rsdp_available(&self) -> bool;

    /// Get the RSDP address.
    fn get_rsdp_address(&self) -> *const c_void;

    /// Set kernel RSP0 in the GDT/TSS for current CPU.
    fn gdt_set_kernel_rsp0(&self, rsp0: u64);

    /// Check if kernel initialization is complete.
    fn is_kernel_initialized(&self) -> bool;

    /// Trigger kernel panic. Never returns.
    fn kernel_panic(&self, msg: *const c_char) -> !;

    /// Graceful shutdown. Never returns.
    fn kernel_shutdown(&self, reason: *const c_char) -> !;

    /// System reboot. Never returns.
    fn kernel_reboot(&self, reason: *const c_char) -> !;

    /// Retrieve IDT gate entry. Returns 0 on success.
    fn idt_get_gate(&self, vector: u8, entry: *mut c_void) -> c_int;
}

/// Cleanup hook trait - for video to register cleanup without sched depending on video.
/// When a task terminates, this hook is called to clean up associated resources.
pub trait TaskCleanupHook: Send + Sync {
    fn on_task_terminate(&self, task_id: u32);
}

/// Opaque Task type for boot callbacks that need typed task pointers.
/// Zero-sized marker to avoid depending on actual Task struct.
#[repr(C)]
pub struct OpaqueTask {
    _private: [u8; 0],
}

/// Scheduler callbacks specifically for boot crate.
/// Subset of scheduler functionality needed during early boot.
pub trait SchedulerForBoot: Send + Sync {
    /// Request reschedule from interrupt context.
    fn request_reschedule_from_interrupt(&self);

    /// Get current task as opaque pointer.
    fn get_current_task(&self) -> *mut OpaqueTask;

    /// Terminate a task by ID.
    fn task_terminate(&self, task_id: u32) -> c_int;
}
