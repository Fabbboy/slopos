#![no_std]
extern crate alloc;

pub const MAX_PATH_LEN: usize = 256;
pub const MAX_NAME_LEN: usize = 32;

pub mod blockdev;
pub mod devfs;
pub mod ext2;
pub mod ext2_vfs;
pub mod fileio;
pub mod pipe;
pub mod pipe_file_ops;
pub mod ramfs;
pub mod vfs;
pub mod vfs_file_ops;

pub mod tests;

#[cfg(test)]
extern crate std;

pub use blockdev::*;
pub use devfs::DevFs;
pub use ext2::*;
pub use ext2_vfs::{ext2_vfs_init_with_callbacks, ext2_vfs_is_initialized};
pub use fileio::*;
pub use ramfs::RamFs;
pub use vfs::{
    FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult, mount,
    vfs_init_builtin_filesystems, vfs_is_initialized,
};
