use slopos_ostd::lock_class;
use slopos_ostd::sync::InitFlag;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;

use crate::devfs::DevFs;
use crate::ext2_vfs::{EXT2_VFS_STATIC, ext2_vfs_is_initialized, ext2_vfs_is_read_only};
use crate::ramfs::RamFs;
use crate::vfs::VfsResult;
use crate::vfs::mount::{MOUNT_RDONLY, mount};

static VFS_INIT: InitFlag = InitFlag::new();

static RAMFS_ROOT_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_ROOT", LOCK_LEVEL_RESOURCE));
static RAMFS_TMP_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_TMP", LOCK_LEVEL_RESOURCE));
static DEVFS_STATIC: DevFs = DevFs::new();

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
