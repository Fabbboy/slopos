use slopos_hermetic::{BootCtx, BspInit};
use slopos_utils::klog_info;

use crate::early_init::{boot_init_priority, boot_mark_initialized};
use slopos_core::exec;
use slopos_core::sched::{
    boot_step_idle_task, boot_step_scheduler_init, boot_step_task_manager_init,
};
use slopos_drivers::virtio_blk;
use slopos_fs::ext2_vfs::EXT2_VFS_STATIC;
use slopos_fs::vfs::{mount, unmount};
use slopos_fs::{
    ext2_vfs_init_with_callbacks, ext2_vfs_is_initialized, vfs_init_builtin_filesystems,
};

fn boot_step_fs_init(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    slopos_fs::fileio_register_tty_ops(&slopos_drivers::tty_file_ops::TTY_FILE_OPS);
    slopos_fs::fileio_register_socket_ops(&slopos_net::socket_file_ops::SOCKET_FILE_OPS);

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
            // The kernel-test phase (drivers/90) runs before this step and
            // may have called `vfs_init_builtin_filesystems` itself; with
            // ext2 not yet ready, that call mounted RamFs at `/` and set
            // the one-shot init flag, so this `vfs_init_builtin_filesystems`
            // returned without mounting ext2. Force-replace any existing
            // root mount with the live ext2 backing so init/utests can
            // resolve `/sbin/init` and `/bin/*`.
            let _ = unmount(b"/");
            match mount(b"/", &EXT2_VFS_STATIC, 0) {
                Ok(_) => {
                    klog_info!("VFS: mounted / (ext2), /tmp (ramfs), /dev (devfs)");
                }
                Err(e) => {
                    klog_info!("VFS: failed to install ext2 root: {:?}", e);
                    return -1;
                }
            }
        } else {
            klog_info!("VFS: mounted /tmp (ramfs), /dev (devfs)");
        }
    } else {
        klog_info!("VFS: failed to mount builtin filesystems");
        return -1;
    }

    0
}

fn boot_step_init_launch(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
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

fn boot_step_mark_kernel_ready_fn(_ctx: &mut BootCtx<'_, BspInit>) {
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
