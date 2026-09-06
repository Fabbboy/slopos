use core::sync::atomic::{AtomicU8, Ordering};

use slopos_ostd::lock_class;
use slopos_ostd::sync::InitFlag;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;

use crate::devfs::DevFs;
use crate::ext2_vfs::{EXT2_VFS_STATIC, ext2_vfs_is_initialized, ext2_vfs_is_read_only};
use crate::ramfs::RamFs;
use crate::vfs::VfsResult;
use crate::vfs::mount::{MOUNT_RDONLY, mount};
use crate::vfs::orphan::{forget_filesystem, has_open_refs};
use crate::vfs::traits::{FileSystem, same_filesystem};

static VFS_INIT: InitFlag = InitFlag::new();

static RAMFS_ROOT_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_ROOT", LOCK_LEVEL_RESOURCE));
static RAMFS_TMP_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_TMP", LOCK_LEVEL_RESOURCE));
static DEVFS_STATIC: DevFs = DevFs::new();

/// How many ramfs instances `mount(2)` may have outstanding at once.
pub const RAMFS_POOL_LEN: usize = 4;

/// Instances `mount(2)` can hand out for `fstype="ramfs"`.
///
/// `mount(2)` cannot conjure a `&'static dyn FileSystem`, so the instances
/// exist up front. Each carries its **own** lock class: a path walk crossing a
/// mount holds one mount's lock while taking the next one's, and a shared
/// class would make that legal nesting look unordered.
static RAMFS_POOL: [RamFs; RAMFS_POOL_LEN] = [
    RamFs::new_const(lock_class!("RAMFS_POOL_0", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_POOL_1", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_POOL_2", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_POOL_3", LOCK_LEVEL_RESOURCE)),
];

const POOL_FREE: u8 = 0;
const POOL_BOUND: u8 = 1;
/// Unmounted lazily while a descriptor still named it. Contents are kept —
/// the descriptor holds a `&'static dyn FileSystem` — until
/// [`reclaim_retired_slots`] finds the reference gone; a later claim is the
/// only thing that needs the slot back, so nothing polls.
const POOL_RETIRED: u8 = 2;

static RAMFS_POOL_STATE: [AtomicU8; RAMFS_POOL_LEN] =
    [const { AtomicU8::new(POOL_FREE) }; RAMFS_POOL_LEN];

fn pool_slot_of(fs: &'static dyn FileSystem) -> Option<usize> {
    RAMFS_POOL
        .iter()
        .position(|candidate| same_filesystem(candidate, fs))
}

/// Return every retired instance whose last reference has gone.
///
/// The `POOL_RETIRED -> POOL_BOUND` step is what makes the cleanup exclusive:
/// a slot published free before it is reset could be claimed in between, and
/// this call would then drop the *new* mount's records.
fn reclaim_retired_slots() {
    for (idx, state) in RAMFS_POOL_STATE.iter().enumerate() {
        let instance: &'static dyn FileSystem = &RAMFS_POOL[idx];
        if state.load(Ordering::Acquire) != POOL_RETIRED
            || has_open_refs(instance)
            || state
                .compare_exchange(
                    POOL_RETIRED,
                    POOL_BOUND,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            continue;
        }
        forget_filesystem(instance);
        RAMFS_POOL[idx].reset();
        state.store(POOL_FREE, Ordering::Release);
    }
}

/// Claim a pooled ramfs for a `mount(2)`, or `None` when all of them are in
/// use. The instance is reset before it is handed out, so a mount never sees
/// the previous one's files.
pub fn vfs_ramfs_pool_claim() -> Option<&'static RamFs> {
    // Swept first rather than as a fallback: a retired instance whose
    // reference is gone is free, and holding it back until the pool is
    // otherwise exhausted loses capacity for the rest of the boot.
    reclaim_retired_slots();

    for (idx, state) in RAMFS_POOL_STATE.iter().enumerate() {
        if state
            .compare_exchange(POOL_FREE, POOL_BOUND, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            RAMFS_POOL[idx].reset();
            return Some(&RAMFS_POOL[idx]);
        }
    }

    None
}

/// Give back the pooled instance `fs` names, answering whether it was one.
///
/// `retire` when a lazy unmount left a live descriptor behind: the instance is
/// kept intact until a later claim finds it idle, because resetting it under a
/// reader would hand that reader an empty filesystem.
pub fn vfs_ramfs_pool_release(fs: &'static dyn FileSystem, retire: bool) -> bool {
    let Some(idx) = pool_slot_of(fs) else {
        return false;
    };
    if retire {
        RAMFS_POOL_STATE[idx].store(POOL_RETIRED, Ordering::Release);
    } else {
        RAMFS_POOL[idx].reset();
        RAMFS_POOL_STATE[idx].store(POOL_FREE, Ordering::Release);
    }
    true
}

/// The one devfs instance, so `mount(2)` can put it at a second path. Device
/// nodes are global here, so both mounts show the same tree.
pub fn vfs_devfs_instance() -> &'static DevFs {
    &DEVFS_STATIC
}

/// What `/` is backed by. The boot step decides; this module never infers it
/// from what happens to be mounted, because a writable disk being present is
/// not the same as it being the root the caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootBacking {
    Ramfs,
    /// The ext2 device, if it is initialised; read-only when it refuses
    /// writes. Falls back to ramfs when no device is initialised.
    Ext2,
}

/// The one-shot mount of `/`, `/tmp` and `/dev`. Later calls are no-ops
/// whatever `root` they pass: the kernel-test phase reaches this first with
/// ramfs, and the boot step re-mounts `/` itself when it wants the disk.
pub fn vfs_init_builtin_filesystems_with(root: RootBacking) -> VfsResult<()> {
    if !VFS_INIT.init_once() {
        return Ok(());
    }

    match root {
        RootBacking::Ext2 if ext2_vfs_is_initialized() => {
            let flags = if ext2_vfs_is_read_only() {
                MOUNT_RDONLY
            } else {
                0
            };
            mount(b"/", &EXT2_VFS_STATIC, flags)?;
        }
        _ => mount(b"/", &RAMFS_ROOT_STATIC, 0)?,
    }

    mount(b"/tmp", &RAMFS_TMP_STATIC, 0)?;
    mount(b"/dev", &DEVFS_STATIC, 0)?;

    Ok(())
}

/// [`vfs_init_builtin_filesystems_with`] on a RAM root: the form every caller
/// that is not the boot step wants.
pub fn vfs_init_builtin_filesystems() -> VfsResult<()> {
    vfs_init_builtin_filesystems_with(RootBacking::Ramfs)
}

pub fn vfs_is_initialized() -> bool {
    VFS_INIT.is_set()
}
