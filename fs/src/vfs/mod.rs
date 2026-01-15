pub mod init;
pub mod mount;
pub mod ops;
pub mod path;
pub mod traits;

pub use init::{vfs_init_builtin_filesystems, vfs_is_initialized};
pub use mount::{mount, unmount, with_mount_table};
pub use ops::{VfsHandle, vfs_list, vfs_mkdir, vfs_open, vfs_stat, vfs_unlink};
pub use path::{ResolvedPath, resolve_parent, resolve_path};
pub use traits::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
