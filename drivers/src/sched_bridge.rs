//! Scheduler bridge - consolidated trait object storage with fail-fast initialization.

use core::ffi::{c_char, c_int, c_void};
use slopos_abi::sched_traits::{
    BootServices, FateResult, OpaqueTask, SchedulerServices, TaskCleanupHook, TaskRef,
};
use spin::Once;

static SCHED: Once<&'static dyn SchedulerServices> = Once::new();
static BOOT: Once<&'static dyn BootServices> = Once::new();
static VIDEO_CLEANUP: Once<fn(u32)> = Once::new();
static CLEANUP_HOOK: Once<&'static dyn TaskCleanupHook> = Once::new();

pub fn init_scheduler(sched: &'static dyn SchedulerServices) {
    SCHED.call_once(|| sched);
}

pub fn init_boot(boot: &'static dyn BootServices) {
    BOOT.call_once(|| boot);
}

pub fn register_cleanup_hook(hook: &'static dyn TaskCleanupHook) {
    CLEANUP_HOOK.call_once(|| hook);
}

#[inline]
fn sched() -> Option<&'static dyn SchedulerServices> {
    SCHED.get().copied()
}

#[inline]
fn boot() -> Option<&'static dyn BootServices> {
    BOOT.get().copied()
}

#[inline]
pub fn is_scheduler_initialized() -> bool {
    SCHED.get().is_some()
}

#[inline]
pub fn is_boot_initialized() -> bool {
    BOOT.get().is_some()
}

pub fn timer_tick() {
    if let Some(s) = sched() {
        s.timer_tick();
    }
}

pub fn handle_post_irq() {
    if let Some(s) = sched() {
        s.handle_post_irq();
    }
}

pub fn request_reschedule_from_interrupt() {
    if let Some(s) = sched() {
        s.request_reschedule_from_interrupt();
    }
}

pub fn get_current_task() -> TaskRef {
    sched()
        .map(|s| s.get_current_task())
        .unwrap_or(TaskRef::NULL)
}

pub fn yield_cpu() {
    if let Some(s) = sched() {
        s.yield_cpu();
    }
}

pub fn schedule() {
    if let Some(s) = sched() {
        s.schedule();
    }
}

pub fn task_terminate(task_id: u32) -> c_int {
    sched().map(|s| s.task_terminate(task_id)).unwrap_or(-1)
}

pub fn block_current_task() {
    if let Some(s) = sched() {
        s.block_current_task();
    }
}

pub fn task_is_blocked(task: TaskRef) -> bool {
    sched().map(|s| s.task_is_blocked(task)).unwrap_or(false)
}

pub fn unblock_task(task: TaskRef) -> c_int {
    sched().map(|s| s.unblock_task(task)).unwrap_or(-1)
}

pub fn scheduler_is_enabled() -> c_int {
    sched().map(|s| s.is_enabled()).unwrap_or(0)
}

pub fn scheduler_is_preemption_enabled() -> c_int {
    sched().map(|s| s.is_preemption_enabled()).unwrap_or(0)
}

pub fn register_idle_wakeup_callback(cb: Option<fn() -> c_int>) {
    if let Some(s) = sched() {
        s.register_idle_wakeup_callback(cb);
    }
}

pub fn get_task_stats(total: *mut u32, active: *mut u32, context_switches: *mut u64) {
    if let Some(s) = sched() {
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

pub fn get_scheduler_stats(
    context_switches: *mut u64,
    yields: *mut u64,
    ready_tasks: *mut u32,
    schedule_calls: *mut u32,
) {
    if let Some(s) = sched() {
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

pub fn fate_spin() -> FateResult {
    sched()
        .map(|s| s.fate_spin())
        .unwrap_or(FateResult { token: 0, value: 0 })
}

pub fn fate_set_pending(res: FateResult, task_id: u32) -> c_int {
    sched()
        .map(|s| s.fate_set_pending(res, task_id))
        .unwrap_or(-1)
}

pub fn fate_take_pending(task_id: u32, out: *mut FateResult) -> c_int {
    if let Some(s) = sched() {
        if let Some(result) = s.fate_take_pending(task_id) {
            if !out.is_null() {
                unsafe { *out = result };
            }
            return 0;
        }
    }
    -1
}

pub fn fate_apply_outcome(res: *const FateResult, resolution: u32, award: bool) {
    if let Some(s) = sched() {
        if !res.is_null() {
            unsafe { s.fate_apply_outcome(&*res, resolution, award) };
        }
    }
}

pub fn is_rsdp_available() -> i32 {
    boot()
        .map(|b| if b.is_rsdp_available() { 1 } else { 0 })
        .unwrap_or(0)
}

pub fn get_rsdp_address() -> *const c_void {
    boot()
        .map(|b| b.get_rsdp_address())
        .unwrap_or(core::ptr::null())
}

pub fn gdt_set_kernel_rsp0(rsp0: u64) {
    if let Some(b) = boot() {
        b.gdt_set_kernel_rsp0(rsp0);
    }
}

pub fn is_kernel_initialized() -> i32 {
    boot()
        .map(|b| if b.is_kernel_initialized() { 1 } else { 0 })
        .unwrap_or(0)
}

pub fn idt_get_gate(vector: u8, entry: *mut c_void) -> c_int {
    boot().map(|b| b.idt_get_gate(vector, entry)).unwrap_or(-1)
}

pub fn kernel_panic(msg: *const c_char) -> ! {
    if let Some(b) = boot() {
        b.kernel_panic(msg)
    }
    loop {
        core::hint::spin_loop();
    }
}

pub fn kernel_shutdown(reason: *const c_char) -> ! {
    if let Some(b) = boot() {
        b.kernel_shutdown(reason)
    }
    loop {
        core::hint::spin_loop();
    }
}

pub fn kernel_reboot(reason: *const c_char) -> ! {
    if let Some(b) = boot() {
        b.kernel_reboot(reason)
    }
    loop {
        core::hint::spin_loop();
    }
}

pub fn boot_request_reschedule_from_interrupt() {
    if let Some(s) = sched() {
        s.request_reschedule_from_interrupt();
    }
}

pub fn boot_get_current_task() -> *mut OpaqueTask {
    sched()
        .map(|s| s.get_current_task_opaque())
        .unwrap_or(core::ptr::null_mut())
}

pub fn boot_task_terminate(task_id: u32) -> c_int {
    sched().map(|s| s.task_terminate(task_id)).unwrap_or(-1)
}

pub fn video_task_cleanup(task_id: u32) {
    if let Some(hook) = CLEANUP_HOOK.get() {
        hook.on_task_terminate(task_id);
        return;
    }
    if let Some(cb) = VIDEO_CLEANUP.get() {
        cb(task_id);
    }
}

pub fn register_video_task_cleanup_callback(callback: fn(u32)) {
    VIDEO_CLEANUP.call_once(|| callback);
}
