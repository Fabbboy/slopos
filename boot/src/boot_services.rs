use core::ffi::{c_int, c_void};

use slopos_lib::{klog_debug, klog_info};

use crate::early_init::boot_init_priority;

#[repr(C)]
struct FramebufferInfo {
    physical_addr: u64,
    virtual_addr: *mut c_void,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u8,
    pixel_format: u8,
    buffer_size: u32,
    initialized: u8,
}

unsafe extern "C" {
    fn boot_mark_initialized();
    fn framebuffer_get_info() -> *mut FramebufferInfo;
    fn framebuffer_is_initialized() -> i32;
    fn boot_step_task_manager_init() -> c_int;
    fn boot_step_scheduler_init() -> c_int;
    fn boot_step_idle_task() -> c_int;
}

extern "C" fn boot_step_task_manager_init_wrapper() -> i32 {
    unsafe { boot_step_task_manager_init() }
}

extern "C" fn boot_step_scheduler_init_wrapper() -> i32 {
    unsafe { boot_step_scheduler_init() }
}

extern "C" fn boot_step_idle_task_wrapper() -> i32 {
    unsafe { boot_step_idle_task() }
}

crate::boot_init_step_with_flags!(
    BOOT_STEP_TASK_MANAGER,
    services,
    b"task manager\0",
    boot_step_task_manager_init_wrapper,
    boot_init_priority(20)
);

crate::boot_init_step_with_flags!(
    BOOT_STEP_SCHEDULER,
    services,
    b"scheduler\0",
    boot_step_scheduler_init_wrapper,
    boot_init_priority(30)
);

crate::boot_init_step_with_flags!(
    BOOT_STEP_IDLE_TASK,
    services,
    b"idle task\0",
    boot_step_idle_task_wrapper,
    boot_init_priority(50)
);

extern "C" fn boot_step_mark_kernel_ready_fn() {
    unsafe { boot_mark_initialized() };
    klog_info!("Kernel core services initialized.");
}

extern "C" fn boot_step_framebuffer_demo_fn() {
    let fb_info = unsafe { framebuffer_get_info() };
    if fb_info.is_null() || unsafe { framebuffer_is_initialized() } == 0 {
        klog_info!("Graphics demo: framebuffer not initialized, skipping");
        return;
    }

    klog_debug!("Graphics demo: framebuffer validation complete");
}

crate::boot_init_step_with_flags_unit!(
    BOOT_STEP_MARK_READY,
    services,
    b"mark ready\0",
    boot_step_mark_kernel_ready_fn,
    boot_init_priority(60)
);

crate::boot_init_optional_step_unit!(
    BOOT_STEP_FRAMEBUFFER_DEMO,
    optional,
    b"framebuffer demo\0",
    boot_step_framebuffer_demo_fn
);
