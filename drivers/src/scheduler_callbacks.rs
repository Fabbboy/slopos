#![allow(dead_code)]

use core::ffi::c_void;

/// Callback functions that the scheduler can register with drivers
/// This breaks the circular dependency between drivers and sched crates
pub struct SchedulerCallbacks {
    pub timer_tick: Option<extern "C" fn()>,
    pub handle_post_irq: Option<extern "C" fn()>,
    pub request_reschedule_from_interrupt: Option<extern "C" fn()>,
    pub get_current_task: Option<extern "C" fn() -> *mut c_void>,
}

static mut CALLBACKS: SchedulerCallbacks = SchedulerCallbacks {
    timer_tick: None,
    handle_post_irq: None,
    request_reschedule_from_interrupt: None,
    get_current_task: None,
};

/// Register scheduler callbacks (called by scheduler during initialization)
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

