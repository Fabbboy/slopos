#![no_std]
#![forbid(unsafe_code)]

pub const MAX_PATH_LEN: usize = 256;
pub const MAX_NAME_LEN: usize = 32;

pub mod blockdev;
pub mod cpio;
pub mod devfs;
pub mod ext2;
pub mod ext2_vfs;
pub mod fileio;
pub mod pipe;
pub mod pipe_file_ops;
pub mod ramfs;
pub mod verity;
pub mod vfs;
pub mod vfs_file_ops;

#[cfg(feature = "tests")]
pub mod tests;

#[cfg(test)]
extern crate std;

pub use blockdev::*;
pub use cpio::{CpioError, unpack_cpio_into_root};
pub use devfs::DevFs;
pub use ext2::*;
pub use ext2_vfs::{
    Ext2MountInfo, ext2_vfs_init_with_device, ext2_vfs_is_initialized, ext2_vfs_is_read_only,
    ext2_vfs_shutdown_sync, ext2_vfs_sync,
};
pub use fileio::*;
pub use ramfs::RamFs;
pub use vfs::{
    FileStat, FileSystem, FileType, InodeId, RootBacking, VfsError, VfsResult, mount,
    vfs_init_builtin_filesystems, vfs_init_builtin_filesystems_with, vfs_is_initialized,
};
