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

pub fn vfs_init_builtin_filesystems() -> VfsResult<()> {
    if !VFS_INIT.init_once() {
        return Ok(());
    }

    if ext2_vfs_is_initialized() {
        let flags = if ext2_vfs_is_read_only() {
            MOUNT_RDONLY
        } else {
            0
        };
        mount(b"/", &EXT2_VFS_STATIC, flags)?;
    } else {
        mount(b"/", &RAMFS_ROOT_STATIC, 0)?;
    }

    mount(b"/tmp", &RAMFS_TMP_STATIC, 0)?;
    mount(b"/dev", &DEVFS_STATIC, 0)?;

    Ok(())
}

pub fn vfs_is_initialized() -> bool {
    VFS_INIT.is_set()
}
