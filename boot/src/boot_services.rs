use slopos_hermetic::{BootCtx, BspInit};
use slopos_ostd::klog_info;

use crate::early_init::{boot_init_priority, boot_mark_initialized};
use slopos_core::exec;
use slopos_sched::scheduler::{
    boot_step_idle_task, boot_step_scheduler_init, boot_step_task_manager_init,
};

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering};

use slopos_drivers::virtio_blk;
use slopos_fs::blockdev::{BlockDevice, BlockDeviceIndex};
use slopos_fs::devfs::devfs_register_block_device;
use slopos_fs::ext2_vfs::EXT2_VFS_STATIC;
use slopos_fs::partition::{
    PartitionDevice, PartitionScheme, PartitionTable, SharedBlockDevice, probe,
};
use slopos_fs::verity::VerityStatus;
use slopos_fs::vfs::{MOUNT_RDONLY, mount, unmount};
use slopos_fs::{
    RootBacking, ext2_vfs_init_with_device, ext2_vfs_is_initialized, ext2_vfs_is_read_only,
    vfs_init_builtin_filesystems_with,
};
use slopos_ostd::sync::{InitFlag, OnceLock};
use slopos_ostd::{KArc, KBox};

/// Selected root-filesystem backing, set from the `root=` cmdline knob by
/// `early_init::boot_step_boot_config_fn` (default [`ROOT_AUTO`]).
pub const ROOT_AUTO: u8 = 0;
pub const ROOT_INITRAMFS: u8 = 1;
pub const ROOT_VIRTIO: u8 = 2;

static ROOT_MODE: AtomicU8 = AtomicU8::new(ROOT_AUTO);

/// `root=/dev/vdX[N]`: the probe-order device index and a 1-based partition
/// (0 = whole device). `u16::MAX` is unset, i.e. disk0 whole-device.
static ROOT_BLOCK_INDEX: AtomicU16 = AtomicU16::new(u16::MAX);
static ROOT_BLOCK_PARTITION: AtomicU8 = AtomicU8::new(0);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RootBlockSpec {
    pub index: BlockDeviceIndex,
    /// 1-based, 0 for the whole device.
    pub partition: u8,
    /// `true` when the boot named this device rather than defaulting to disk0.
    pub explicit: bool,
}

/// Naming a device does not force the disk root the way `root=virtio` does —
/// an absent device or partition degrades to the initramfs as no disk does —
/// so this sets no [`ROOT_MODE`].
pub fn set_root_block_device(index: u16, partition: u8) {
    ROOT_BLOCK_INDEX.store(index, Ordering::Relaxed);
    ROOT_BLOCK_PARTITION.store(partition, Ordering::Relaxed);
}

fn root_block_spec() -> RootBlockSpec {
    let raw = ROOT_BLOCK_INDEX.load(Ordering::Relaxed);
    RootBlockSpec {
        index: BlockDeviceIndex(if raw == u16::MAX { 0 } else { raw }),
        partition: ROOT_BLOCK_PARTITION.load(Ordering::Relaxed),
        explicit: raw != u16::MAX,
    }
}

/// `verity=require`: a disk that is attached must come up verified, or the
/// boot step fails. No disk at all is the initramfs-only case and passes. A
/// check that can switch itself off without saying so is not a check; this is
/// how a boot asserts it is running one.
static VERITY_REQUIRED: AtomicBool = AtomicBool::new(false);

pub fn set_verity_required(required: bool) {
    VERITY_REQUIRED.store(required, Ordering::Relaxed);
}

/// Set once the initramfs is unpacked, so [`boot_step_fs_init`] demotes the ext2
/// disk to a `/mnt` secondary instead of replacing `/`.
static ROOTFS_IS_RAMFS: AtomicBool = AtomicBool::new(false);

pub fn set_root_mode(mode: u8) {
    ROOT_MODE.store(mode, Ordering::Relaxed);
}

static FS_HOOKS_INIT: InitFlag = InitFlag::new();

/// Idempotent: either the initramfs or the virtio path may reach the VFS first.
fn register_fs_hooks() {
    if !FS_HOOKS_INIT.init_once() {
        return;
    }
    slopos_fs::fileio_register_tty_ops(&slopos_drivers::tty_file_ops::TTY_FILE_OPS);
    slopos_fs::fileio_register_socket_ops(&slopos_net::socket_file_ops::SOCKET_FILE_OPS);
    slopos_mm::filemap_hook::filemap_register_ops(slopos_fs::filemap::filemap_ops());
}

/// Bring up the RAM-resident root from a Limine-loaded initramfs (cpio) module.
///
/// Runs before [`boot_step_fs_init`]; a no-op on the virtio path, where that
/// step mounts the ext2 disk at `/` instead.
fn boot_step_rootfs_init(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    let archive = crate::limine_protocol::initramfs();
    // Before the decision, not after: `root=auto` picks the disk, so the
    // outcome of attaching it is the input to the choice.
    let disk = attach_disk0_once();

    let mounted = matches!(disk, DiskAttachOutcome::Mounted(_));
    let writable_disk = matches!(disk, DiskAttachOutcome::Mounted(info) if !info.read_only);
    let use_initramfs = match ROOT_MODE.load(Ordering::Relaxed) {
        ROOT_INITRAMFS => true,
        ROOT_VIRTIO => false,
        // A read-only disk falls back to the initramfs as a no-disk boot does:
        // nothing written to such a root survives, so preferring it buys no
        // persistence and costs a writable `/`. That is what keeps `just boot`
        // working, whose shipped image is verified and therefore read-only.
        // With no initramfs to fall back to, a read-only disk is still a root.
        _ => !writable_disk && archive.is_some(),
    };
    if !use_initramfs {
        if !mounted {
            klog_info!("ROOTFS: no initramfs module and no mountable disk — no root to install");
            return -1;
        }
        klog_info!(
            "ROOTFS: root=disk — / is the ext2 disk{}",
            if writable_disk {
                ", and what it holds persists"
            } else {
                " (read-only)"
            }
        );
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

    // Explicitly ramfs: a writable disk may be initialised by now, and the
    // unpack must never land on it.
    if vfs_init_builtin_filesystems_with(RootBacking::Ramfs).is_err() {
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

/// Mount flags for the ext2 disk: read-only when the filesystem or its device
/// refuses writes, so the refusal reaches userland as `EROFS` at the VFS.
fn ext2_mount_flags() -> u32 {
    if ext2_vfs_is_read_only() {
        MOUNT_RDONLY
    } else {
        0
    }
}

/// Why disk0 did not come up verified. Under `verity=require` every arm is a
/// failed boot step, not just the one that reached the trailer parse.
#[derive(Debug, Clone, Copy)]
enum DiskAttachOutcome {
    NoDisk,
    NotReady,
    Unclaimable,
    NoMemory,
    MountFailed,
    Mounted(slopos_fs::Ext2MountInfo),
}

/// The attach verdict, computed once: two boot steps need it, and
/// [`attach_disk0`] claims the device's exclusive write capability, which a
/// second call could not take.
static DISK0_ATTACH: OnceLock<DiskAttachOutcome> = OnceLock::new();

fn attach_disk0_once() -> DiskAttachOutcome {
    DISK0_ATTACH.call_once(attach_disk0);
    DISK0_ATTACH
        .get()
        .copied()
        .unwrap_or(DiskAttachOutcome::NoDisk)
}

/// The root device's single claim: the exclusive write capability can only be
/// taken once, so the mount and the `/dev` node both delegate to this.
static ROOT_BLOCK_DEVICE: OnceLock<KArc<dyn BlockDevice + Send + Sync>> = OnceLock::new();

fn attach_disk0() -> DiskAttachOutcome {
    let spec = root_block_spec();
    let Some(handle) = virtio_blk::blk_device_by_index(spec.index) else {
        if spec.explicit {
            klog_info!(
                "FS: root= named block device {} but no such virtio-blk device is present — \
                 degrading as though there were no disk",
                spec.index.0
            );
        }
        return DiskAttachOutcome::NoDisk;
    };
    if !virtio_blk::blk_is_ready(handle) {
        return DiskAttachOutcome::NotReady;
    }
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => {
            klog_info!(
                "FS: could not claim disk{} write capability: {:?}",
                spec.index.0,
                e
            );
            return DiskAttachOutcome::Unclaimable;
        }
    };
    let Ok(owned) = KArc::try_new(token) else {
        return DiskAttachOutcome::NoMemory;
    };
    let shared: KArc<dyn BlockDevice + Send + Sync> = owned;
    ROOT_BLOCK_DEVICE.call_once(|| shared.clone());

    let device = match root_mount_device(&shared, spec) {
        Ok(d) => d,
        Err(outcome) => return outcome,
    };
    match ext2_vfs_init_with_device(device) {
        Ok(info) => DiskAttachOutcome::Mounted(info),
        Err(e) => {
            klog_info!(
                "FS: virtio-blk disk{} found but ext2 init failed: {:?}",
                spec.index.0,
                e
            );
            DiskAttachOutcome::MountFailed
        }
    }
}

/// The window ext2 mounts: the whole device unless `root=` named a partition.
fn root_mount_device(
    shared: &KArc<dyn BlockDevice + Send + Sync>,
    spec: RootBlockSpec,
) -> Result<KBox<dyn BlockDevice + Send + Sync>, DiskAttachOutcome> {
    if spec.partition == 0 {
        let Ok(boxed) = KBox::try_new(SharedBlockDevice(shared.clone())) else {
            return Err(DiskAttachOutcome::NoMemory);
        };
        return Ok(boxed);
    }

    let table = match probe(shared.as_ref()) {
        Ok(t) => t,
        Err(e) => {
            klog_info!(
                "FS: root= named partition {} of disk{} but its partition table is unusable \
                 ({:?}) — degrading as though there were no disk",
                spec.partition,
                spec.index.0,
                e
            );
            return Err(DiskAttachOutcome::NoDisk);
        }
    };
    let Some(entry) = table.find(spec.partition) else {
        klog_info!(
            "FS: root= named partition {} of disk{} but the {:?} table has no such partition — \
             degrading as though there were no disk",
            spec.partition,
            spec.index.0,
            table.scheme
        );
        return Err(DiskAttachOutcome::NoDisk);
    };
    let window = match PartitionDevice::try_new(shared.clone(), entry.start, entry.len) {
        Ok(w) => w,
        Err(e) => {
            klog_info!("FS: partition {} is unusable: {:?}", spec.partition, e);
            return Err(DiskAttachOutcome::NoDisk);
        }
    };
    let Ok(boxed) = KBox::try_new(window) else {
        return Err(DiskAttachOutcome::NoMemory);
    };
    klog_info!(
        "FS: root is partition {} of disk{} ({} bytes at offset {})",
        spec.partition,
        spec.index.0,
        entry.len,
        entry.start
    );
    Ok(boxed)
}

/// Publish `/dev/vd<letter>` for every probed virtio-blk device and
/// `/dev/vd<letter><n>` for its partitions. A registration failure is only a
/// warning: a missing `/dev` node does not stop an already-mounted kernel.
fn register_block_device_nodes() {
    let root = root_block_spec();
    // One letter per device, `vda`..`vdz`.
    let count = virtio_blk::blk_device_count().min(26);
    for index in 0..count as u16 {
        let Some(handle) = virtio_blk::blk_device_by_index(BlockDeviceIndex(index)) else {
            continue;
        };
        if !virtio_blk::blk_is_ready(handle) {
            continue;
        }
        // The root device's claim is exclusive, so its node shares the mount's
        // `KArc` instead of opening a second view.
        let whole: KArc<dyn BlockDevice + Send + Sync> = match ROOT_BLOCK_DEVICE.get() {
            Some(shared) if index == root.index.0 => shared.clone(),
            _ => match KArc::try_new(virtio_blk::BlockReader::new(handle)) {
                Ok(reader) => reader,
                Err(_) => {
                    klog_info!("DEVFS: out of memory registering block device {}", index);
                    continue;
                }
            },
        };

        let letter = b'a' + index as u8;
        let mut name = [b'v', b'd', letter, 0, 0, 0];
        if let Err(e) = devfs_register_block_device(&name[..3], whole.clone()) {
            klog_info!("DEVFS: could not register /dev/vd{}: {:?}", index, e);
            continue;
        }

        let table = match probe(whole.as_ref()) {
            Ok(t) => t,
            Err(e) => {
                klog_info!("DEVFS: /dev/vd{} partition table unusable: {:?}", index, e);
                continue;
            }
        };
        if table.scheme == PartitionScheme::None {
            continue;
        }
        register_partition_nodes(&whole, &table, &mut name);
    }
}

fn register_partition_nodes(
    whole: &KArc<dyn BlockDevice + Send + Sync>,
    table: &PartitionTable,
    name: &mut [u8; 6],
) {
    for entry in table.entries.iter() {
        let len = partition_node_name(name, entry.number);
        let window = match PartitionDevice::try_new(whole.clone(), entry.start, entry.len) {
            Ok(w) => w,
            Err(e) => {
                klog_info!("DEVFS: partition {} unusable: {:?}", entry.number, e);
                continue;
            }
        };
        let device: KArc<dyn BlockDevice + Send + Sync> = match KArc::try_new(window) {
            Ok(d) => d,
            Err(_) => {
                klog_info!("DEVFS: out of memory registering a partition node");
                continue;
            }
        };
        if let Err(e) = devfs_register_block_device(&name[..len], device) {
            klog_info!(
                "DEVFS: could not register partition {}: {:?}",
                entry.number,
                e
            );
        }
    }
}

/// Appends `number` in decimal after the `vd<letter>` prefix in `name`,
/// answering the new length. Three digits: a GPT table may hold 128 entries.
fn partition_node_name(name: &mut [u8; 6], number: u8) -> usize {
    let mut len = 3;
    if number >= 100 {
        name[len] = b'0' + number / 100;
        len += 1;
    }
    if number >= 10 {
        name[len] = b'0' + (number / 10) % 10;
        len += 1;
    }
    name[len] = b'0' + number % 10;
    len + 1
}

fn boot_step_fs_init(_ctx: &mut BootCtx<'_, BspInit>) -> i32 {
    register_fs_hooks();

    let outcome = attach_disk0_once();
    if let DiskAttachOutcome::Mounted(info) = outcome {
        klog_info!(
            "FS: ext2 initialized from virtio-blk disk0 ({}, verity {})",
            if info.read_only {
                "read-only"
            } else {
                "read-write"
            },
            match info.verity {
                VerityStatus::Absent => "absent",
                VerityStatus::Verified { .. } => "enabled",
                VerityStatus::VerifiedWritable { .. } => "enabled (writable)",
            },
        );
        if info.orphans_drained > 0 {
            klog_info!(
                "FS: reclaimed {} inode(s) the previous boot left unlinked-but-open",
                info.orphans_drained
            );
        }
        if info.check_overdue {
            klog_info!(
                "FS: the image is due a full check by its own mount-count or check-interval \
                 rule — run `e2fsck -f` on the host"
            );
        }
    }
    register_block_device_nodes();
    // Absence is not an error: on real hardware the root came from the
    // initramfs. A disk that is there, though, must come up verified when the
    // boot said so — and a disk that is there but could not be mounted is
    // exactly the case the knob exists to catch.
    if VERITY_REQUIRED.load(Ordering::Relaxed) {
        let verified = matches!(
            outcome,
            DiskAttachOutcome::NoDisk
                | DiskAttachOutcome::Mounted(slopos_fs::Ext2MountInfo {
                    verity: VerityStatus::Verified { .. },
                    ..
                })
                | DiskAttachOutcome::Mounted(slopos_fs::Ext2MountInfo {
                    verity: VerityStatus::VerifiedWritable { .. },
                    ..
                })
        );
        if !verified {
            klog_info!(
                "FS: verity=require but disk0 is not verified: {:?}",
                outcome
            );
            return -1;
        }
    }

    if ROOTFS_IS_RAMFS.load(Ordering::Relaxed) {
        if ext2_vfs_is_initialized() {
            let flags = ext2_mount_flags();
            match mount(b"/mnt", &EXT2_VFS_STATIC, flags) {
                Ok(_) => klog_info!(
                    "VFS: mounted ext2 at /mnt (secondary, {})",
                    if flags & MOUNT_RDONLY != 0 {
                        "read-only"
                    } else {
                        "read-write"
                    },
                ),
                Err(e) => klog_info!("VFS: failed to mount ext2 at /mnt: {:?}", e),
            }
        }
        return 0;
    }

    if vfs_init_builtin_filesystems_with(RootBacking::Ext2).is_ok() {
        if ext2_vfs_is_initialized() {
            // The kernel-test phase may already have mounted RamFs at `/` and
            // tripped the one-shot init flag, so the call above returned without
            // mounting ext2.
            let _ = unmount(b"/");
            let flags = ext2_mount_flags();
            match mount(b"/", &EXT2_VFS_STATIC, flags) {
                Ok(_) => {
                    if flags & MOUNT_RDONLY == 0 {
                        slopos_ostd::boot_flags::set_flag(
                            slopos_ostd::boot_flags::BOOT_FLAG_ROOT_PERSISTENT,
                        );
                    }
                    klog_info!(
                        "VFS: mounted / (ext2, {}), /tmp (ramfs), /dev (devfs)",
                        if flags & MOUNT_RDONLY != 0 {
                            "read-only"
                        } else {
                            "read-write"
                        },
                    );
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
