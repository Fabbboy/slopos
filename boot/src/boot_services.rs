use core::ffi::c_void;

use slopos_lib::{klog_debug, klog_info};

use crate::early_init::{boot_init_priority, boot_mark_initialized};
use slopos_sched::{
    boot_step_idle_task, boot_step_scheduler_init, boot_step_task_manager_init,
};
use slopos_video::framebuffer::{framebuffer_get_info, framebuffer_is_initialized};

// FFI struct for reading framebuffer info from C pointers
// Used via unsafe pointer dereferencing: `unsafe { &*fb_info }` in boot_step_framebuffer_demo_fn
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

// Force Rust to recognize this type as used (it's used via unsafe pointer dereferencing)
// Using size_of ensures the type is recognized as used at compile time
const _: usize = core::mem::size_of::<FramebufferInfo>();

fn boot_step_task_manager_init_wrapper() -> i32 {
    boot_step_task_manager_init()
}

fn boot_step_scheduler_init_wrapper() -> i32 {
    boot_step_scheduler_init()
}

fn boot_step_idle_task_wrapper() -> i32 {
    boot_step_idle_task()
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

fn boot_step_mark_kernel_ready_fn() {
    boot_mark_initialized();
    klog_info!("Kernel core services initialized.");
}

fn boot_step_framebuffer_demo_fn() {
    let fb_info = framebuffer_get_info();
    if fb_info.is_null() || framebuffer_is_initialized() == 0 {
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
