use slopos_lib::{klog_debug, klog_info};

use crate::early_init::{boot_init_priority, boot_mark_initialized};
use slopos_core::{boot_step_idle_task, boot_step_scheduler_init, boot_step_task_manager_init};
use slopos_drivers::virtio_blk;
use slopos_fs::{
    ext2_vfs_init_with_callbacks, ext2_vfs_is_initialized, vfs_init_builtin_filesystems,
};
use slopos_video::framebuffer::{framebuffer_is_initialized, get_display_info};

fn boot_step_task_manager_init_wrapper() -> i32 {
    boot_step_task_manager_init()
}

fn boot_step_scheduler_init_wrapper() -> i32 {
    boot_step_scheduler_init()
}

fn boot_step_idle_task_wrapper() -> i32 {
    boot_step_idle_task()
}

fn boot_step_fs_init() -> i32 {
    if virtio_blk::virtio_blk_is_ready() {
        if ext2_vfs_init_with_callbacks(
            virtio_blk::virtio_blk_read,
            virtio_blk::virtio_blk_write,
            virtio_blk::virtio_blk_capacity,
        )
        .is_ok()
        {
            klog_info!("FS: ext2 initialized from virtio-blk");
        } else {
            klog_info!("FS: virtio-blk found but ext2 init failed");
        }
    }

    if vfs_init_builtin_filesystems().is_ok() {
        if ext2_vfs_is_initialized() {
            klog_info!("VFS: mounted / (ext2), /tmp (ramfs), /dev (devfs)");
        } else {
            klog_info!("VFS: mounted /tmp (ramfs), /dev (devfs)");
        }
    } else {
        klog_info!("VFS: failed to mount builtin filesystems");
        return -1;
    }

    0
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

crate::boot_init_step_with_flags!(
    BOOT_STEP_FS_INIT,
    services,
    b"fs init\0",
    boot_step_fs_init,
    boot_init_priority(55)
);

fn boot_step_mark_kernel_ready_fn() {
    boot_mark_initialized();
    klog_info!("Kernel core services initialized.");
}

fn boot_step_framebuffer_demo_fn() {
    if get_display_info().is_none() || framebuffer_is_initialized() == 0 {
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
