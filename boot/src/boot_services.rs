use slopos_lib::{klog_debug, klog_info};

use crate::early_init::{boot_init_priority, boot_mark_initialized};
use slopos_sched::{boot_step_idle_task, boot_step_scheduler_init, boot_step_task_manager_init};
use slopos_video::framebuffer::{framebuffer_get_info, framebuffer_is_initialized};

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
    b"wheel of fate\0",
    boot_step_framebuffer_demo_fn
);
