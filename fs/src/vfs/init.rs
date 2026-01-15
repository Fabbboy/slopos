use core::sync::atomic::{AtomicBool, Ordering};

use crate::devfs::DevFs;
use crate::ext2_vfs::{EXT2_VFS_STATIC, ext2_vfs_is_initialized};
use crate::ramfs::RamFs;
use crate::vfs::VfsResult;
use crate::vfs::mount::mount;

static VFS_INITIALIZED: AtomicBool = AtomicBool::new(false);

static RAMFS_STATIC: RamFs = RamFs::new_const();
static DEVFS_STATIC: DevFs = DevFs::new();

pub fn vfs_init_builtin_filesystems() -> VfsResult<()> {
    if VFS_INITIALIZED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }

    if ext2_vfs_is_initialized() {
        mount(b"/", &EXT2_VFS_STATIC, 0)?;
    }

    mount(b"/tmp", &RAMFS_STATIC, 0)?;
    mount(b"/dev", &DEVFS_STATIC, 0)?;

    Ok(())
}

pub fn vfs_is_initialized() -> bool {
    VFS_INITIALIZED.load(Ordering::Acquire)
}
