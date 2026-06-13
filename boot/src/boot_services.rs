use slopos_hermetic::{BootCtx, BspInit};
use slopos_ostd::klog_info;

use crate::early_init::{boot_init_priority, boot_mark_initialized};
use slopos_core::exec;
use slopos_sched::scheduler::{
    boot_step_idle_task, boot_step_scheduler_init, boot_step_task_manager_init,
};

fn boot_step_register_scheduler_fn(ctx: &mut BootCtx<'_, BspInit>) {
    // Tell OSTD where the kernel scheduler and idle-task factory
    // live. Both hooks are one-shot — `register_*` panics on second
    // call — and must complete before AP bring-up triggers idle-task
    // creation via `current_idle_task_factory()`.
    slopos_ostd::task::register_scheduler(
        &ctx.bsp_token(),
        slopos_sched::per_cpu::scheduler_handle(),
    );
    slopos_ostd::task::register_idle_task_factory(
        &ctx.bsp_token(),
        slopos_sched::runtime::create_idle_task_for_cpu,
    );
    klog_info!("OSTD: scheduler registered (PriorityScheduler)");
}
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use slopos_drivers::virtio_blk;
use slopos_fs::blockdev::{BlockDevice, BlockDeviceIndex};
use slopos_fs::ext2_vfs::EXT2_VFS_STATIC;
use slopos_fs::vfs::{mount, unmount};
use slopos_fs::{ext2_vfs_init_with_device, ext2_vfs_is_initialized, vfs_init_builtin_filesystems};
use slopos_ostd::KBox;
use slopos_ostd::sync::InitFlag;

/// Selected root-filesystem backing, set from the `root=` cmdline knob by
/// `early_init::boot_step_boot_config_fn` (default [`ROOT_AUTO`]).
pub const ROOT_AUTO: u8 = 0;
pub const ROOT_INITRAMFS: u8 = 1;
pub const ROOT_VIRTIO: u8 = 2;

static ROOT_MODE: AtomicU8 = AtomicU8::new(ROOT_AUTO);

/// Set true once the initramfs has been unpacked into the RAM root, so
/// [`boot_step_fs_init`] demotes the ext2 disk to a `/mnt` secondary instead of
/// replacing `/`.
static ROOTFS_IS_RAMFS: AtomicBool = AtomicBool::new(false);

/// Record the `root=` selection parsed from the kernel cmdline.
pub fn set_root_mode(mode: u8) {
    ROOT_MODE.store(mode, Ordering::Relaxed);
}

static FS_HOOKS_INIT: InitFlag = InitFlag::new();

/// Register the filesystem-related task/file hooks exactly once, regardless of
/// whether the initramfs or the virtio path brings the VFS up first.
fn register_fs_hooks() {
    if !FS_HOOKS_INIT.init_once() {
        return;
    }
    slopos_fs::fileio_register_tty_ops(&slopos_drivers::tty_file_ops::TTY_FILE_OPS);
    slopos_fs::fileio_register_socket_ops(&slopos_net::socket_file_ops::SOCKET_FILE_OPS);

    // Release any poll/select wait-queue references a task still holds when it
    // dies. A task SIGKILL'd while blocked in poll() never runs its own
    // unregister path, so without this its registered OpenFiles would keep an
    // extra refcount forever and their backends (e.g. unix_close → peer EOF)
    // would never fire. Tied to task termination, mirroring the futex cleanup.
    slopos_sched::task::register_task_resource_cleanup_hook(slopos_fs::fileio_poll_cleanup_task);

    // Release any in-flight SCM_RIGHTS fd refs a task was holding across a
    // blocking unix_sendmsg() when it was killed. Same abandon-stack rationale
    // as the poll hook above.
    slopos_sched::task::register_task_resource_cleanup_hook(
        slopos_net::unix_socket::unix_inflight_cleanup_task,
    );
}

/// Bring up the RAM-resident root from a Limine-loaded initramfs (cpio) module.
///
/// Runs before [`boot_step_fs_init`]. On the initramfs path it mounts the
/// builtin filesystems (RamFs at `/`, ramfs `/tmp`, devfs `/dev`) and unpacks
/// the cpio into `/`, so the whole userland boots from RAM with no storage
/// driver — identical in QEMU and on real hardware. On the virtio path it is a
/// no-op and `boot_step_fs_init` mounts the ext2 disk at `/` as before.
fn boot_step_rootfs_init(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    let archive = crate::limine_protocol::initramfs();
    let use_initramfs = match ROOT_MODE.load(Ordering::Relaxed) {
        ROOT_INITRAMFS => true,
        ROOT_VIRTIO => false,
        _ => archive.is_some(), // ROOT_AUTO: initramfs iff a module is present
    };
    if !use_initramfs {
        return 0;
    }

    let archive = match archive {
        Some(bytes) => bytes,
        None => {
            klog_info!("ROOTFS: root=initramfs requested but no initramfs module present");
            return -1;
        }
    };

    register_fs_hooks();

    // ext2 is not initialized at this priority, so this mounts RamFs at `/`
    // plus ramfs `/tmp` and devfs `/dev`. (If the kernel-test phase already
    // mounted the builtin filesystems, this is a no-op and the RamFs root is
    // reused.)
    if vfs_init_builtin_filesystems().is_err() {
        klog_info!("ROOTFS: failed to mount builtin filesystems");
        return -1;
    }

    match slopos_fs::unpack_cpio_into_root(archive) {
        Ok(entries) => {
            klog_info!(
                "ROOTFS: unpacked {} initramfs entries ({} bytes) into RAM root",
                entries,
                archive.len()
            );
        }
        Err(e) => {
            klog_info!("ROOTFS: initramfs unpack failed: {:?}", e);
            return -1;
        }
    }

    ROOTFS_IS_RAMFS.store(true, Ordering::Relaxed);
    0
}

fn boot_step_fs_init(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    register_fs_hooks();

    // Initialize ext2 from disk0 (the first-probed virtio-blk device) if one is
    // attached. The FS acquires an EXCLUSIVE write capability and holds the
    // token for the kernel's lifetime, so nothing else can open a second writer
    // to the device (Layer 1: ownership = exclusion). On real hardware there is
    // no such disk — that's fine, the root came from the initramfs.
    if let Some(disk0) = virtio_blk::blk_device_by_index(BlockDeviceIndex(0)) {
        if virtio_blk::blk_is_ready(disk0) {
            match virtio_blk::open_writer(disk0) {
                Ok(token) => match KBox::try_new(token) {
                    Ok(boxed) => {
                        let device: KBox<dyn BlockDevice + Send + Sync> = boxed;
                        if ext2_vfs_init_with_device(device).is_ok() {
                            klog_info!("FS: ext2 initialized from virtio-blk disk0");
                        } else {
                            klog_info!("FS: virtio-blk disk0 found but ext2 init failed");
                        }
                    }
                    Err(_) => klog_info!("FS: failed to allocate block-device handle for disk0"),
                },
                Err(e) => {
                    klog_info!("FS: could not claim disk0 write capability: {:?}", e)
                }
            }
        }
    }

    // Initramfs path: the RAM root is already mounted at `/`. Demote the ext2
    // disk (if present) to a `/mnt` secondary — the future installer's target —
    // and never touch `/`.
    if ROOTFS_IS_RAMFS.load(Ordering::Relaxed) {
        if ext2_vfs_is_initialized() {
            match mount(b"/mnt", &EXT2_VFS_STATIC, 0) {
                Ok(_) => klog_info!("VFS: mounted ext2 at /mnt (secondary)"),
                Err(e) => klog_info!("VFS: failed to mount ext2 at /mnt: {:?}", e),
            }
        }
        return 0;
    }

    // Virtio path: mount the builtin filesystems and install ext2 at `/`.
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
    BOOT_STEP_REGISTER_SCHEDULER,
    services,
    b"register scheduler with OSTD\0",
    boot_step_register_scheduler_fn,
    flags = boot_init_priority(35)
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
    BOOT_STEP_ROOTFS_INIT,
    services,
    b"initramfs root\0",
    boot_step_rootfs_init,
    fallible,
    flags = boot_init_priority(54)
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
