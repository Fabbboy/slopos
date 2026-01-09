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

static mut TIMING: Option<&'static dyn SchedulerTiming> = None;
static mut EXECUTION: Option<&'static dyn SchedulerExecution> = None;
static mut STATE: Option<&'static dyn SchedulerState> = None;
static mut FATE: Option<&'static dyn SchedulerFate> = None;
static mut BOOT: Option<&'static dyn BootServices> = None;
static mut SCHED_FOR_BOOT: Option<&'static dyn SchedulerForBoot> = None;
static mut CLEANUP_HOOK: Option<&'static dyn TaskCleanupHook> = None;
static mut VIDEO_CLEANUP_FN: Option<fn(u32)> = None;

// =============================================================================
// Macro for generating wrapper functions
// =============================================================================

macro_rules! sched_fn {
    // Void function, no args
    ($name:ident, $static:ident, $method:ident()) => {
        pub fn $name() {
            if let Some(t) = unsafe { $static } { t.$method(); }
        }
    };
    // Void function with args
    ($name:ident, $static:ident, $method:ident($($arg:ident: $ty:ty),+)) => {
        pub fn $name($($arg: $ty),+) {
            if let Some(t) = unsafe { $static } { t.$method($($arg),+); }
        }
    };
    // Function returning value, no args
    ($name:ident, $static:ident, $method:ident() -> $ret:ty, $default:expr) => {
        pub fn $name() -> $ret {
            unsafe { $static }.map(|t| t.$method()).unwrap_or($default)
        }
    };
    // Function returning value with args
    ($name:ident, $static:ident, $method:ident($($arg:ident: $ty:ty),+) -> $ret:ty, $default:expr) => {
        pub fn $name($($arg: $ty),+) -> $ret {
            unsafe { $static }.map(|t| t.$method($($arg),+)).unwrap_or($default)
        }
    };
    // Bool to i32 conversion, no args
    ($name:ident, $static:ident, $method:ident() -> bool_as_i32) => {
        pub fn $name() -> i32 {
            unsafe { $static }.map(|t| if t.$method() { 1 } else { 0 }).unwrap_or(0)
        }
    };
}

// =============================================================================
// Registration Functions (kept manual - called once during boot)
// =============================================================================

/// Register all scheduler traits (called once by sched during init).
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
pub unsafe fn register_boot_services(boot: &'static dyn BootServices) {
    BOOT = Some(boot);
}

/// Register scheduler callbacks for boot (called by sched during init).
pub unsafe fn register_scheduler_for_boot(sched: &'static dyn SchedulerForBoot) {
    SCHED_FOR_BOOT = Some(sched);
}

/// Register cleanup hook (called by video crate).
pub unsafe fn register_cleanup_hook(hook: &'static dyn TaskCleanupHook) {
    CLEANUP_HOOK = Some(hook);
}

// =============================================================================
// SchedulerTiming wrappers
// =============================================================================

sched_fn!(timer_tick, TIMING, timer_tick());
sched_fn!(handle_post_irq, TIMING, handle_post_irq());
sched_fn!(request_reschedule_from_interrupt, TIMING, request_reschedule_from_interrupt());

// =============================================================================
// SchedulerExecution wrappers
// =============================================================================

sched_fn!(get_current_task, EXECUTION, get_current_task() -> TaskHandle, core::ptr::null_mut());
sched_fn!(yield_cpu, EXECUTION, yield_cpu());
sched_fn!(schedule, EXECUTION, schedule());
sched_fn!(task_terminate, EXECUTION, task_terminate(task_id: u32) -> c_int, -1);
sched_fn!(block_current_task, EXECUTION, block_current_task());
sched_fn!(task_is_blocked, EXECUTION, task_is_blocked(task: TaskHandle) -> bool, false);
sched_fn!(unblock_task, EXECUTION, unblock_task(task: TaskHandle) -> c_int, -1);

// =============================================================================
// SchedulerState wrappers
// =============================================================================

sched_fn!(scheduler_is_enabled, STATE, is_enabled() -> c_int, 0);
sched_fn!(scheduler_is_preemption_enabled, STATE, is_preemption_enabled() -> c_int, 0);
sched_fn!(register_idle_wakeup_callback, STATE, register_idle_wakeup_callback(cb: Option<fn() -> c_int>));

/// Get task statistics via out-pointers (legacy API).
pub fn get_task_stats(total: *mut u32, active: *mut u32, context_switches: *mut u64) {
    if let Some(s) = unsafe { STATE } {
        let (t, a, cs) = s.get_task_stats();
        if !total.is_null() { unsafe { *total = t }; }
        if !active.is_null() { unsafe { *active = a }; }
        if !context_switches.is_null() { unsafe { *context_switches = cs }; }
    }
}

/// Get scheduler statistics via out-pointers (legacy API).
pub fn get_scheduler_stats(
    context_switches: *mut u64,
    yields: *mut u64,
    ready_tasks: *mut u32,
    schedule_calls: *mut u32,
) {
    if let Some(s) = unsafe { STATE } {
        let (cs, y, rt, sc) = s.get_scheduler_stats();
        if !context_switches.is_null() { unsafe { *context_switches = cs }; }
        if !yields.is_null() { unsafe { *yields = y }; }
        if !ready_tasks.is_null() { unsafe { *ready_tasks = rt }; }
        if !schedule_calls.is_null() { unsafe { *schedule_calls = sc }; }
    }
}

// =============================================================================
// SchedulerFate wrappers (Wheel of Fate)
// =============================================================================

sched_fn!(fate_spin, FATE, fate_spin() -> FateResult, FateResult { token: 0, value: 0 });
sched_fn!(fate_set_pending, FATE, fate_set_pending(res: FateResult, task_id: u32) -> c_int, -1);

/// Retrieve and clear pending fate (returns -1 if none pending).
pub fn fate_take_pending(task_id: u32, out: *mut FateResult) -> c_int {
    if let Some(f) = unsafe { FATE } {
        if let Some(result) = f.fate_take_pending(task_id) {
            if !out.is_null() { unsafe { *out = result }; }
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
// BootServices wrappers
// =============================================================================

// Note: HHDM functions (get_hhdm_offset, is_hhdm_available) removed.
// Drivers should use slopos_mm::hhdm module directly instead.

sched_fn!(is_rsdp_available, BOOT, is_rsdp_available() -> bool_as_i32);
sched_fn!(get_rsdp_address, BOOT, get_rsdp_address() -> *const c_void, core::ptr::null());
sched_fn!(gdt_set_kernel_rsp0, BOOT, gdt_set_kernel_rsp0(rsp0: u64));
sched_fn!(is_kernel_initialized, BOOT, is_kernel_initialized() -> bool_as_i32);
sched_fn!(idt_get_gate, BOOT, idt_get_gate(vector: u8, entry: *mut c_void) -> c_int, -1);

/// Trigger kernel panic. Never returns.
pub fn kernel_panic(msg: *const c_char) -> ! {
    if let Some(b) = unsafe { BOOT } { b.kernel_panic(msg) }
    loop { core::hint::spin_loop(); }
}

/// Graceful shutdown. Never returns.
pub fn kernel_shutdown(reason: *const c_char) -> ! {
    if let Some(b) = unsafe { BOOT } { b.kernel_shutdown(reason) }
    loop { core::hint::spin_loop(); }
}

/// System reboot. Never returns.
pub fn kernel_reboot(reason: *const c_char) -> ! {
    if let Some(b) = unsafe { BOOT } { b.kernel_reboot(reason) }
    loop { core::hint::spin_loop(); }
}

// =============================================================================
// SchedulerForBoot wrappers
// =============================================================================

sched_fn!(boot_request_reschedule_from_interrupt, SCHED_FOR_BOOT, request_reschedule_from_interrupt());
sched_fn!(boot_get_current_task, SCHED_FOR_BOOT, get_current_task() -> *mut OpaqueTask, core::ptr::null_mut());
sched_fn!(boot_task_terminate, SCHED_FOR_BOOT, task_terminate(task_id: u32) -> c_int, -1);

// =============================================================================
// Task Cleanup Hook
// =============================================================================

/// Call the registered video task cleanup callback.
pub fn video_task_cleanup(task_id: u32) {
    if let Some(hook) = unsafe { CLEANUP_HOOK } {
        hook.on_task_terminate(task_id);
        return;
    }
    if let Some(cb) = unsafe { VIDEO_CLEANUP_FN } { cb(task_id); }
}

/// Register a cleanup callback using the legacy function pointer API.
pub fn register_video_task_cleanup_callback(callback: fn(u32)) {
    unsafe { VIDEO_CLEANUP_FN = Some(callback); }
}
