use slopos_utils::klog_info;

use crate::early_init::{boot_init_priority, boot_mark_initialized};
use slopos_core::exec;
use slopos_core::sched::{
    boot_step_idle_task, boot_step_scheduler_init, boot_step_task_manager_init,
};
use slopos_drivers::virtio_blk;
use slopos_fs::{
    ext2_vfs_init_with_callbacks, ext2_vfs_is_initialized, vfs_init_builtin_filesystems,
};

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

fn boot_step_init_launch() -> i32 {
    match exec::launch_init() {
        Ok(task_id) => {
            klog_info!("USERLAND: launched /sbin/init as task {}", task_id);
            0
        }
        Err(err) => {
            klog_info!("USERLAND: failed to launch /sbin/init ({:?})", err);
            -1
        }
    }
}

crate::boot_init!(
    BOOT_STEP_TASK_MANAGER,
    services,
    b"task manager\0",
    boot_step_task_manager_init,
    fallible,
    flags = boot_init_priority(20)
);
crate::boot_init!(
    BOOT_STEP_SCHEDULER,
    services,
    b"scheduler\0",
    boot_step_scheduler_init,
    fallible,
    flags = boot_init_priority(30)
);
crate::boot_init!(
    BOOT_STEP_IDLE_TASK,
    services,
    b"idle task\0",
    boot_step_idle_task,
    fallible,
    flags = boot_init_priority(50)
);
crate::boot_init!(
    BOOT_STEP_FS_INIT,
    services,
    b"fs init\0",
    boot_step_fs_init,
    fallible,
    flags = boot_init_priority(55)
);
crate::boot_init!(
    BOOT_STEP_INIT_LAUNCH,
    services,
    b"launch /sbin/init\0",
    boot_step_init_launch,
    fallible,
    flags = boot_init_priority(58)
);

fn boot_step_mark_kernel_ready_fn() {
    boot_mark_initialized();
    klog_info!("Kernel core services initialized.");
}

crate::boot_init!(
    BOOT_STEP_MARK_READY,
    services,
    b"mark ready\0",
    boot_step_mark_kernel_ready_fn,
    flags = boot_init_priority(60)
);
