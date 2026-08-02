use slopos_ostd::lock_class;
use slopos_ostd::sync::InitFlag;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;

use crate::devfs::DevFs;
use crate::ext2_vfs::{EXT2_VFS_STATIC, ext2_vfs_is_initialized};
use crate::ramfs::RamFs;
use crate::vfs::VfsResult;
use crate::vfs::mount::mount;

static VFS_INIT: InitFlag = InitFlag::new();

static RAMFS_ROOT_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_ROOT", LOCK_LEVEL_RESOURCE));
static RAMFS_TMP_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_TMP", LOCK_LEVEL_RESOURCE));
static DEVFS_STATIC: DevFs = DevFs::new();

pub fn vfs_init_builtin_filesystems() -> VfsResult<()> {
    if !VFS_INIT.init_once() {
        return Ok(());
    }

    if ext2_vfs_is_initialized() {
        mount(b"/", &EXT2_VFS_STATIC, 0)?;
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
