pub mod canon;
pub mod init;
pub mod mount;
pub mod ops;
pub mod orphan;
pub mod path;
pub mod traits;

pub use canon::{CanonPath, canonicalise};
pub use init::{vfs_init_builtin_filesystems, vfs_is_initialized};
pub use mount::{MAX_MOUNTS, MOUNT_RDONLY, Mounted, mount, mount_at, unmount, with_mount_table};
pub use ops::{
    ListCursor, VfsHandle, VfsOpenFlags, vfs_list, vfs_list_from, vfs_mkdir, vfs_open,
    vfs_open_flags, vfs_readlink, vfs_rename, vfs_rmdir, vfs_set_mode, vfs_set_sealed, vfs_stat,
    vfs_symlink, vfs_sync_all, vfs_unlink,
};
pub use path::{ResolvedPath, resolve_parent, resolve_path};
pub use traits::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
