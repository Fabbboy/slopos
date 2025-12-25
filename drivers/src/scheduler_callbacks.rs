#![allow(improper_ctypes)]

use crate::fate::FateResult;
use core::ffi::{c_char, c_int, c_void};

/// Task type for boot callbacks (opaque pointer to avoid dependency on sched)
#[repr(C)]
pub struct Task {
    _private: [u8; 0],
}

/// Callback functions that the scheduler can register with drivers
/// This breaks the circular dependency between drivers and sched crates
#[repr(C)]
pub struct SchedulerCallbacks {
    pub timer_tick: Option<fn()>,
    pub handle_post_irq: Option<fn()>,
    pub request_reschedule_from_interrupt: Option<fn()>,
    pub get_current_task: Option<fn() -> *mut c_void>,
    pub yield_fn: Option<fn()>,
    pub schedule_fn: Option<fn()>,
    pub task_terminate_fn: Option<fn(u32) -> c_int>,
    pub scheduler_is_preemption_enabled_fn: Option<fn() -> c_int>,
    pub get_task_stats_fn: Option<fn(*mut u32, *mut u32, *mut u64)>,
    pub get_scheduler_stats_fn: Option<fn(*mut u64, *mut u64, *mut u32, *mut u32)>,
    pub register_idle_wakeup_callback: Option<fn(Option<fn() -> c_int>)>,
    pub scheduler_is_enabled: Option<fn() -> c_int>,
    pub task_is_blocked: Option<fn(*const c_void) -> bool>,
    pub unblock_task: Option<fn(*mut c_void) -> c_int>,
    pub block_current_task: Option<fn()>,
    pub fate_spin: Option<fn() -> FateResult>,
    pub fate_set_pending: Option<fn(FateResult, u32) -> c_int>,
    pub fate_take_pending: Option<fn(u32, *mut FateResult) -> c_int>,
    pub fate_apply_outcome: Option<fn(*const FateResult, u32, bool)>,
}

static mut CALLBACKS: SchedulerCallbacks = SchedulerCallbacks {
    timer_tick: None,
    handle_post_irq: None,
    request_reschedule_from_interrupt: None,
    get_current_task: None,
    yield_fn: None,
    schedule_fn: None,
    task_terminate_fn: None,
    scheduler_is_preemption_enabled_fn: None,
    get_task_stats_fn: None,
    get_scheduler_stats_fn: None,
    register_idle_wakeup_callback: None,
    scheduler_is_enabled: None,
    task_is_blocked: None,
    unblock_task: None,
    block_current_task: None,
    fate_spin: None,
    fate_set_pending: None,
    fate_take_pending: None,
    fate_apply_outcome: None,
};

/// Register scheduler callbacks (called by scheduler during initialization)
#[allow(improper_ctypes_definitions)]
pub unsafe fn register_callbacks(callbacks: SchedulerCallbacks) {
    CALLBACKS = callbacks;
}

/// Call the registered timer tick callback
pub unsafe fn call_timer_tick() {
    if let Some(cb) = CALLBACKS.timer_tick {
        cb();
    }
}

/// Call the registered post-IRQ handler callback
pub unsafe fn call_handle_post_irq() {
    if let Some(cb) = CALLBACKS.handle_post_irq {
        cb();
    }
}

/// Call the registered reschedule request callback
pub unsafe fn call_request_reschedule_from_interrupt() {
    if let Some(cb) = CALLBACKS.request_reschedule_from_interrupt {
        cb();
    }
}

/// Call the registered get current task callback
pub unsafe fn call_get_current_task() -> *mut c_void {
    if let Some(cb) = CALLBACKS.get_current_task {
        cb()
    } else {
        core::ptr::null_mut()
    }
}

/// Call the registered yield callback
pub unsafe fn call_yield() {
    if let Some(cb) = CALLBACKS.yield_fn {
        cb();
    }
}

/// Call the registered schedule callback
pub unsafe fn call_schedule() {
    if let Some(cb) = CALLBACKS.schedule_fn {
        cb();
    }
}

/// Call the registered task terminate callback
pub unsafe fn call_task_terminate(task_id: u32) -> c_int {
    if let Some(cb) = CALLBACKS.task_terminate_fn {
        cb(task_id)
    } else {
        -1
    }
}

/// Call the registered scheduler is preemption enabled callback
pub unsafe fn call_scheduler_is_preemption_enabled() -> c_int {
    if let Some(cb) = CALLBACKS.scheduler_is_preemption_enabled_fn {
        cb()
    } else {
        0
    }
}

/// Call the registered get task stats callback
pub unsafe fn call_get_task_stats(total: *mut u32, active: *mut u32, context_switches: *mut u64) {
    if let Some(cb) = CALLBACKS.get_task_stats_fn {
        cb(total, active, context_switches);
    }
}

/// Call the registered get scheduler stats callback
pub unsafe fn call_get_scheduler_stats(
    context_switches: *mut u64,
    yields: *mut u64,
    ready_tasks: *mut u32,
    schedule_calls: *mut u32,
) {
    if let Some(cb) = CALLBACKS.get_scheduler_stats_fn {
        cb(context_switches, yields, ready_tasks, schedule_calls);
    }
}

pub unsafe fn call_register_idle_wakeup_callback(callback: Option<fn() -> c_int>) {
    if let Some(cb) = CALLBACKS.register_idle_wakeup_callback {
        cb(callback);
    }
}

pub unsafe fn call_scheduler_is_enabled() -> c_int {
    if let Some(cb) = CALLBACKS.scheduler_is_enabled {
        cb()
    } else {
        0
    }
}

pub unsafe fn call_task_is_blocked(task: *const c_void) -> bool {
    if let Some(cb) = CALLBACKS.task_is_blocked {
        cb(task)
    } else {
        false
    }
}

pub unsafe fn call_unblock_task(task: *mut c_void) -> c_int {
    if let Some(cb) = CALLBACKS.unblock_task {
        cb(task)
    } else {
        -1
    }
}

pub unsafe fn call_block_current_task() {
    if let Some(cb) = CALLBACKS.block_current_task {
        cb();
    }
}

pub unsafe fn call_fate_spin() -> FateResult {
    if let Some(cb) = CALLBACKS.fate_spin {
        cb()
    } else {
        FateResult { token: 0, value: 0 }
    }
}

pub unsafe fn call_fate_set_pending(res: FateResult, task_id: u32) -> c_int {
    if let Some(cb) = CALLBACKS.fate_set_pending {
        cb(res, task_id)
    } else {
        -1
    }
}

pub unsafe fn call_fate_take_pending(task_id: u32, out: *mut FateResult) -> c_int {
    if let Some(cb) = CALLBACKS.fate_take_pending {
        cb(task_id, out)
    } else {
        -1
    }
}

pub unsafe fn call_fate_apply_outcome(res: *const FateResult, resolution: u32, award: bool) {
    if let Some(cb) = CALLBACKS.fate_apply_outcome {
        cb(res, resolution, award);
    }
}

/// Callback functions that the scheduler can register for boot to use
/// This breaks the circular dependency between boot and sched crates
#[repr(C)]
pub struct SchedulerCallbacksForBoot {
    pub request_reschedule_from_interrupt: Option<fn()>,
    pub get_current_task: Option<fn() -> *mut Task>,
    pub task_terminate: Option<fn(u32) -> c_int>,
}

/// Callback functions that boot can register for other crates to use
/// This breaks circular dependencies between boot and other crates
#[repr(C)]
pub struct BootCallbacks {
    pub gdt_set_kernel_rsp0: Option<fn(u64)>,
    pub is_kernel_initialized: Option<fn() -> i32>,
    pub kernel_panic: Option<fn(*const c_char)>,
    pub kernel_shutdown: Option<fn(*const c_char)>,
    pub kernel_reboot: Option<fn(*const c_char)>,
    pub get_hhdm_offset: Option<fn() -> u64>,
    pub is_hhdm_available: Option<fn() -> i32>,
    pub is_rsdp_available: Option<fn() -> i32>,
    pub get_rsdp_address: Option<fn() -> *const c_void>,
    pub idt_get_gate: Option<fn(u8, *mut c_void) -> c_int>,
}

static mut BOOT_CALLBACKS: SchedulerCallbacksForBoot = SchedulerCallbacksForBoot {
    request_reschedule_from_interrupt: None,
    get_current_task: None,
    task_terminate: None,
};

static mut BOOT_REGISTERED_CALLBACKS: BootCallbacks = BootCallbacks {
    gdt_set_kernel_rsp0: None,
    is_kernel_initialized: None,
    kernel_panic: None,
    kernel_shutdown: None,
    kernel_reboot: None,
    get_hhdm_offset: None,
    is_hhdm_available: None,
    is_rsdp_available: None,
    get_rsdp_address: None,
    idt_get_gate: None,
};

/// Register scheduler callbacks for boot (called by scheduler during initialization)
#[allow(improper_ctypes_definitions)]
pub unsafe fn register_scheduler_callbacks_for_boot(callbacks: SchedulerCallbacksForBoot) {
    BOOT_CALLBACKS = callbacks;
}

/// Call the registered request reschedule from interrupt callback
pub unsafe fn call_boot_request_reschedule_from_interrupt() {
    if let Some(cb) = BOOT_CALLBACKS.request_reschedule_from_interrupt {
        cb();
    }
}

/// Call the registered get current task callback
pub unsafe fn call_boot_get_current_task() -> *mut Task {
    if let Some(cb) = BOOT_CALLBACKS.get_current_task {
        cb()
    } else {
        core::ptr::null_mut()
    }
}

/// Call the registered task terminate callback
pub unsafe fn call_boot_task_terminate(task_id: u32) -> c_int {
    if let Some(cb) = BOOT_CALLBACKS.task_terminate {
        cb(task_id)
    } else {
        -1
    }
}

/// Register boot callbacks (called during boot initialization)
pub unsafe fn register_boot_callbacks(callbacks: BootCallbacks) {
    BOOT_REGISTERED_CALLBACKS = callbacks;
}

/// Call the registered gdt_set_kernel_rsp0 callback
pub unsafe fn call_gdt_set_kernel_rsp0(rsp0: u64) {
    if let Some(cb) = BOOT_REGISTERED_CALLBACKS.gdt_set_kernel_rsp0 {
        cb(rsp0);
    }
}

/// Call the registered is_kernel_initialized callback
pub unsafe fn call_is_kernel_initialized() -> i32 {
    if let Some(cb) = BOOT_REGISTERED_CALLBACKS.is_kernel_initialized {
        cb()
    } else {
        0
    }
}

/// Call the registered kernel_panic callback
pub unsafe fn call_kernel_panic(msg: *const c_char) -> ! {
    if let Some(cb) = BOOT_REGISTERED_CALLBACKS.kernel_panic {
        cb(msg);
        // Functions should never return, but if they do, fall through to halt
    }
    // Fallback: infinite loop if panic callback not registered or if it returned
    loop {
        core::hint::spin_loop();
    }
}

/// Call the registered kernel_shutdown callback
pub unsafe fn call_kernel_shutdown(reason: *const c_char) -> ! {
    if let Some(cb) = BOOT_REGISTERED_CALLBACKS.kernel_shutdown {
        cb(reason);
        // Functions should never return, but if they do, fall through to halt
    }
    // Fallback: infinite loop if shutdown callback not registered or if it returned
    loop {
        core::hint::spin_loop();
    }
}

/// Call the registered kernel_reboot callback
pub unsafe fn call_kernel_reboot(reason: *const c_char) -> ! {
    if let Some(cb) = BOOT_REGISTERED_CALLBACKS.kernel_reboot {
        cb(reason);
        // Functions should never return, but if they do, fall through to halt
    }
    // Fallback: infinite loop if reboot callback not registered or if it returned
    loop {
        core::hint::spin_loop();
    }
}

/// Call the registered idt_get_gate callback
pub unsafe fn call_idt_get_gate(vector: u8, entry: *mut c_void) -> c_int {
    if let Some(cb) = BOOT_REGISTERED_CALLBACKS.idt_get_gate {
        cb(vector, entry)
    } else {
        -1
    }
}

/// Call the registered get_hhdm_offset callback
pub unsafe fn call_get_hhdm_offset() -> u64 {
    if let Some(cb) = BOOT_REGISTERED_CALLBACKS.get_hhdm_offset {
        cb()
    } else {
        0
    }
}

/// Call the registered is_hhdm_available callback
pub unsafe fn call_is_hhdm_available() -> i32 {
    if let Some(cb) = BOOT_REGISTERED_CALLBACKS.is_hhdm_available {
        cb()
    } else {
        0
    }
}

/// Call the registered is_rsdp_available callback
pub unsafe fn call_is_rsdp_available() -> i32 {
    if let Some(cb) = BOOT_REGISTERED_CALLBACKS.is_rsdp_available {
        cb()
    } else {
        0
    }
}

/// Call the registered get_rsdp_address callback
pub unsafe fn call_get_rsdp_address() -> *const c_void {
    if let Some(cb) = BOOT_REGISTERED_CALLBACKS.get_rsdp_address {
        cb()
    } else {
        core::ptr::null()
    }
}

// =============================================================================
// Video/Surface Cleanup Callbacks
// =============================================================================
// These callbacks allow the video crate to hook into task cleanup without
// creating a circular dependency (sched -> video is not allowed since
// video -> sched already exists)

/// Callback for cleaning up video/surface resources when a task terminates
static mut VIDEO_TASK_CLEANUP_CALLBACK: Option<fn(u32)> = None;

/// Register a callback to be called when a task terminates.
/// Used by the video crate to clean up surface resources.
pub fn register_video_task_cleanup_callback(callback: fn(u32)) {
    unsafe {
        VIDEO_TASK_CLEANUP_CALLBACK = Some(callback);
    }
}

/// Call the registered video task cleanup callback.
/// Called by the scheduler when terminating a task.
pub fn call_video_task_cleanup(task_id: u32) {
    unsafe {
        if let Some(cb) = VIDEO_TASK_CLEANUP_CALLBACK {
            cb(task_id);
        }
    }
}
