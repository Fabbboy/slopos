
#![allow(improper_ctypes)]

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
};

/// Register scheduler callbacks (called by scheduler during initialization)
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn register_callbacks(callbacks: SchedulerCallbacks) {
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
};

/// Register scheduler callbacks for boot (called by scheduler during initialization)
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn register_scheduler_callbacks_for_boot(callbacks: SchedulerCallbacksForBoot) {
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
#[unsafe(no_mangle)]
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

/// Callback functions that fs can register for drivers to use
/// This breaks the circular dependency between drivers and fs crates
#[repr(C)]
pub struct FsCallbacks {
    pub file_open_for_process: Option<fn(u32, *const c_char, u32) -> c_int>,
    pub file_close_fd: Option<fn(u32, c_int) -> c_int>,
    pub file_read_fd: Option<fn(u32, c_int, *mut c_char, usize) -> c_int>,
    pub file_write_fd: Option<fn(u32, c_int, *const c_char, usize) -> c_int>,
    pub file_unlink_path: Option<fn(*const c_char) -> c_int>,
}

static mut FS_CALLBACKS: FsCallbacks = FsCallbacks {
    file_open_for_process: None,
    file_close_fd: None,
    file_read_fd: None,
    file_write_fd: None,
    file_unlink_path: None,
};

/// Register fs callbacks (called by fs during initialization)
#[unsafe(no_mangle)]
pub unsafe fn register_fs_callbacks(callbacks: FsCallbacks) {
    FS_CALLBACKS = callbacks;
}

/// Call the registered file_open_for_process callback
pub unsafe fn call_file_open_for_process(process_id: u32, path: *const c_char, flags: u32) -> c_int {
    if let Some(cb) = FS_CALLBACKS.file_open_for_process {
        cb(process_id, path, flags)
    } else {
        -1
    }
}

/// Call the registered file_close_fd callback
pub unsafe fn call_file_close_fd(process_id: u32, fd: c_int) -> c_int {
    if let Some(cb) = FS_CALLBACKS.file_close_fd {
        cb(process_id, fd)
    } else {
        -1
    }
}

/// Call the registered file_read_fd callback
pub unsafe fn call_file_read_fd(process_id: u32, fd: c_int, buffer: *mut c_char, len: usize) -> c_int {
    if let Some(cb) = FS_CALLBACKS.file_read_fd {
        cb(process_id, fd, buffer, len)
    } else {
        -1
    }
}

/// Call the registered file_write_fd callback
pub unsafe fn call_file_write_fd(process_id: u32, fd: c_int, buffer: *const c_char, len: usize) -> c_int {
    if let Some(cb) = FS_CALLBACKS.file_write_fd {
        cb(process_id, fd, buffer, len)
    } else {
        -1
    }
}

/// Call the registered file_unlink_path callback
pub unsafe fn call_file_unlink_path(path: *const c_char) -> c_int {
    if let Some(cb) = FS_CALLBACKS.file_unlink_path {
        cb(path)
    } else {
        -1
    }
}

