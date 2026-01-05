//! Scheduler bridge - stores trait objects and provides call-through functions.
//!
//! This replaces scheduler_callbacks.rs with a type-safe trait-based approach.
//! Traits are defined in `abi/sched_traits.rs`, implementations live in `sched/` and `boot/`.

use core::ffi::{c_char, c_int, c_void};
use slopos_abi::sched_traits::{
    BootServices, FateResult, OpaqueTask, SchedulerExecution, SchedulerFate, SchedulerForBoot,
    SchedulerState, SchedulerTiming, TaskCleanupHook, TaskHandle,
};

// =============================================================================
// Static Storage for Trait Objects
// =============================================================================
// We use static mut with write-once semantics: written during single-threaded boot,
// read after initialization is complete. This is a common pattern in OS kernels.

static mut TIMING: Option<&'static dyn SchedulerTiming> = None;
static mut EXECUTION: Option<&'static dyn SchedulerExecution> = None;
static mut STATE: Option<&'static dyn SchedulerState> = None;
static mut FATE: Option<&'static dyn SchedulerFate> = None;
static mut BOOT: Option<&'static dyn BootServices> = None;
static mut SCHED_FOR_BOOT: Option<&'static dyn SchedulerForBoot> = None;
static mut CLEANUP_HOOK: Option<&'static dyn TaskCleanupHook> = None;

// Legacy function pointer for video cleanup (backwards compatibility)
static mut VIDEO_CLEANUP_FN: Option<fn(u32)> = None;

// =============================================================================
// Registration Functions
// =============================================================================

/// Register all scheduler traits (called once by sched during init).
/// # Safety
/// Must be called during single-threaded boot before scheduler starts.
pub unsafe fn register_scheduler(
    timing: &'static dyn SchedulerTiming,
    execution: &'static dyn SchedulerExecution,
    state: &'static dyn SchedulerState,
    fate: &'static dyn SchedulerFate,
) {
    TIMING = Some(timing);
    EXECUTION = Some(execution);
    STATE = Some(state);
    FATE = Some(fate);
}

/// Register boot services (called once by boot during early init).
/// # Safety
/// Must be called during single-threaded boot.
pub unsafe fn register_boot_services(boot: &'static dyn BootServices) {
    BOOT = Some(boot);
}

/// Register scheduler callbacks for boot (called by sched during init).
/// # Safety
/// Must be called during single-threaded boot.
pub unsafe fn register_scheduler_for_boot(sched: &'static dyn SchedulerForBoot) {
    SCHED_FOR_BOOT = Some(sched);
}

/// Register cleanup hook (called by video crate).
/// # Safety
/// Must be called during initialization.
pub unsafe fn register_cleanup_hook(hook: &'static dyn TaskCleanupHook) {
    CLEANUP_HOOK = Some(hook);
}

// =============================================================================
// Public API - SchedulerTiming
// =============================================================================

/// Called on each timer tick interrupt.
pub fn timer_tick() {
    // SAFETY: Read-only after boot init completes
    if let Some(t) = unsafe { TIMING } {
        t.timer_tick();
    }
}

/// Called after IRQ dispatch completes.
pub fn handle_post_irq() {
    if let Some(t) = unsafe { TIMING } {
        t.handle_post_irq();
    }
}

/// Request reschedule from interrupt context.
pub fn request_reschedule_from_interrupt() {
    if let Some(t) = unsafe { TIMING } {
        t.request_reschedule_from_interrupt();
    }
}

// =============================================================================
// Public API - SchedulerExecution
// =============================================================================

/// Get currently running task (null if none).
pub fn get_current_task() -> TaskHandle {
    unsafe { EXECUTION }
        .map(|e| e.get_current_task())
        .unwrap_or(core::ptr::null_mut())
}

/// Voluntarily yield CPU to scheduler.
pub fn yield_cpu() {
    if let Some(e) = unsafe { EXECUTION } {
        e.yield_cpu();
    }
}

/// Invoke scheduler to pick next task.
pub fn schedule() {
    if let Some(e) = unsafe { EXECUTION } {
        e.schedule();
    }
}

/// Terminate a task by ID. Returns 0 on success.
pub fn task_terminate(task_id: u32) -> c_int {
    unsafe { EXECUTION }
        .map(|e| e.task_terminate(task_id))
        .unwrap_or(-1)
}

/// Block the currently running task.
pub fn block_current_task() {
    if let Some(e) = unsafe { EXECUTION } {
        e.block_current_task();
    }
}

/// Check if a task is blocked.
pub fn task_is_blocked(task: TaskHandle) -> bool {
    unsafe { EXECUTION }
        .map(|e| e.task_is_blocked(task))
        .unwrap_or(false)
}

/// Unblock a task. Returns 0 on success.
pub fn unblock_task(task: TaskHandle) -> c_int {
    unsafe { EXECUTION }.map(|e| e.unblock_task(task)).unwrap_or(-1)
}

// =============================================================================
// Public API - SchedulerState
// =============================================================================

/// Check if scheduler is enabled.
pub fn scheduler_is_enabled() -> c_int {
    unsafe { STATE }.map(|s| s.is_enabled()).unwrap_or(0)
}

/// Check if preemption is enabled.
pub fn scheduler_is_preemption_enabled() -> c_int {
    unsafe { STATE }.map(|s| s.is_preemption_enabled()).unwrap_or(0)
}

/// Get task statistics via out-pointers (legacy API compatibility).
pub fn get_task_stats(total: *mut u32, active: *mut u32, context_switches: *mut u64) {
    if let Some(s) = unsafe { STATE } {
        let (t, a, cs) = s.get_task_stats();
        if !total.is_null() {
            unsafe { *total = t };
        }
        if !active.is_null() {
            unsafe { *active = a };
        }
        if !context_switches.is_null() {
            unsafe { *context_switches = cs };
        }
    }
}

/// Get scheduler statistics via out-pointers (legacy API compatibility).
pub fn get_scheduler_stats(
    context_switches: *mut u64,
    yields: *mut u64,
    ready_tasks: *mut u32,
    schedule_calls: *mut u32,
) {
    if let Some(s) = unsafe { STATE } {
        let (cs, y, rt, sc) = s.get_scheduler_stats();
        if !context_switches.is_null() {
            unsafe { *context_switches = cs };
        }
        if !yields.is_null() {
            unsafe { *yields = y };
        }
        if !ready_tasks.is_null() {
            unsafe { *ready_tasks = rt };
        }
        if !schedule_calls.is_null() {
            unsafe { *schedule_calls = sc };
        }
    }
}

/// Register callback for idle task wakeup.
pub fn register_idle_wakeup_callback(cb: Option<fn() -> c_int>) {
    if let Some(s) = unsafe { STATE } {
        s.register_idle_wakeup_callback(cb);
    }
}

// =============================================================================
// Public API - SchedulerFate (Wheel of Fate)
// =============================================================================

/// Spin the wheel, get a fate result.
pub fn fate_spin() -> FateResult {
    unsafe { FATE }
        .map(|f| f.fate_spin())
        .unwrap_or(FateResult { token: 0, value: 0 })
}

/// Store pending fate for a task.
pub fn fate_set_pending(res: FateResult, task_id: u32) -> c_int {
    unsafe { FATE }
        .map(|f| f.fate_set_pending(res, task_id))
        .unwrap_or(-1)
}

/// Retrieve and clear pending fate (returns -1 if none pending).
pub fn fate_take_pending(task_id: u32, out: *mut FateResult) -> c_int {
    if let Some(f) = unsafe { FATE } {
        if let Some(result) = f.fate_take_pending(task_id) {
            if !out.is_null() {
                unsafe { *out = result };
            }
            return 0;
        }
    }
    -1
}

/// Apply fate outcome (award W or L).
pub fn fate_apply_outcome(res: *const FateResult, resolution: u32, award: bool) {
    if let Some(f) = unsafe { FATE } {
        if !res.is_null() {
            unsafe { f.fate_apply_outcome(&*res, resolution, award) };
        }
    }
}

// =============================================================================
// Public API - BootServices
// =============================================================================

/// Get the Higher Half Direct Map offset.
pub fn get_hhdm_offset() -> u64 {
    unsafe { BOOT }.map(|b| b.get_hhdm_offset()).unwrap_or(0)
}

/// Check if HHDM is available.
pub fn is_hhdm_available() -> i32 {
    unsafe { BOOT }
        .map(|b| if b.is_hhdm_available() { 1 } else { 0 })
        .unwrap_or(0)
}

/// Check if ACPI RSDP is available.
pub fn is_rsdp_available() -> i32 {
    unsafe { BOOT }
        .map(|b| if b.is_rsdp_available() { 1 } else { 0 })
        .unwrap_or(0)
}

/// Get the RSDP address.
pub fn get_rsdp_address() -> *const c_void {
    unsafe { BOOT }
        .map(|b| b.get_rsdp_address())
        .unwrap_or(core::ptr::null())
}

/// Set kernel RSP0 in the GDT/TSS.
pub fn gdt_set_kernel_rsp0(rsp0: u64) {
    if let Some(b) = unsafe { BOOT } {
        b.gdt_set_kernel_rsp0(rsp0);
    }
}

/// Check if kernel initialization is complete.
pub fn is_kernel_initialized() -> i32 {
    unsafe { BOOT }
        .map(|b| if b.is_kernel_initialized() { 1 } else { 0 })
        .unwrap_or(0)
}

/// Trigger kernel panic. Never returns.
pub fn kernel_panic(msg: *const c_char) -> ! {
    if let Some(b) = unsafe { BOOT } {
        b.kernel_panic(msg)
    }
    // Fallback: infinite loop if panic not registered
    loop {
        core::hint::spin_loop();
    }
}

/// Graceful shutdown. Never returns.
pub fn kernel_shutdown(reason: *const c_char) -> ! {
    if let Some(b) = unsafe { BOOT } {
        b.kernel_shutdown(reason)
    }
    loop {
        core::hint::spin_loop();
    }
}

/// System reboot. Never returns.
pub fn kernel_reboot(reason: *const c_char) -> ! {
    if let Some(b) = unsafe { BOOT } {
        b.kernel_reboot(reason)
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Retrieve IDT gate entry.
pub fn idt_get_gate(vector: u8, entry: *mut c_void) -> c_int {
    unsafe { BOOT }.map(|b| b.idt_get_gate(vector, entry)).unwrap_or(-1)
}

// =============================================================================
// Public API - SchedulerForBoot
// =============================================================================

/// Request reschedule from boot context.
pub fn boot_request_reschedule_from_interrupt() {
    if let Some(s) = unsafe { SCHED_FOR_BOOT } {
        s.request_reschedule_from_interrupt();
    }
}

/// Get current task as opaque pointer (for boot).
pub fn boot_get_current_task() -> *mut OpaqueTask {
    unsafe { SCHED_FOR_BOOT }
        .map(|s| s.get_current_task())
        .unwrap_or(core::ptr::null_mut())
}

/// Terminate task from boot context.
pub fn boot_task_terminate(task_id: u32) -> c_int {
    unsafe { SCHED_FOR_BOOT }
        .map(|s| s.task_terminate(task_id))
        .unwrap_or(-1)
}

// =============================================================================
// Public API - Task Cleanup Hook
// =============================================================================

/// Call the registered video task cleanup callback.
pub fn video_task_cleanup(task_id: u32) {
    // First try trait-based hook
    if let Some(hook) = unsafe { CLEANUP_HOOK } {
        hook.on_task_terminate(task_id);
        return;
    }
    // Fall back to legacy function pointer
    if let Some(cb) = unsafe { VIDEO_CLEANUP_FN } {
        cb(task_id);
    }
}

/// Register a cleanup callback using the legacy function pointer API.
pub fn register_video_task_cleanup_callback(callback: fn(u32)) {
    unsafe {
        VIDEO_CLEANUP_FN = Some(callback);
    }
}
