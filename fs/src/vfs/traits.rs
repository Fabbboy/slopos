//! VFS trait definitions — the abstractions every filesystem implementation
//! must adhere to.

/// Each filesystem maintains its own inode number space.
pub type InodeId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileType {
    Regular = 1,
    Directory = 2,
    CharDevice = 3,
    BlockDevice = 4,
    Symlink = 5,
    Pipe = 6,
    Socket = 7,
}

#[derive(Debug, Clone)]
pub struct FileStat {
    pub inode: InodeId,
    pub file_type: FileType,
    pub size: u64,
    pub mode: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub dev_major: u32,
    pub dev_minor: u32,
    /// The inode refuses every mutation: write, truncate, unlink, rename,
    /// being renamed over, and any change to its mode. Set once when the
    /// initramfs is unpacked and never cleared — program-identity privilege is
    /// keyed on a binary's path, so without this any task could overwrite
    /// `/bin/compositor` and spawn the replacement into the grant.
    pub sealed: bool,
}

impl FileStat {
    pub const fn new_file(inode: InodeId, size: u64) -> Self {
        Self {
            inode,
            file_type: FileType::Regular,
            size,
            mode: 0o644,
            nlink: 1,
            uid: 0,
            gid: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            dev_major: 0,
            dev_minor: 0,
            sealed: false,
        }
    }

    pub const fn new_directory(inode: InodeId) -> Self {
        Self {
            inode,
            file_type: FileType::Directory,
            size: 0,
            mode: 0o755,
            nlink: 2,
            uid: 0,
            gid: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            dev_major: 0,
            dev_minor: 0,
            sealed: false,
        }
    }

    pub const fn new_char_device(inode: InodeId, major: u32, minor: u32) -> Self {
        Self {
            inode,
            file_type: FileType::CharDevice,
            size: 0,
            mode: 0o666,
            nlink: 1,
            uid: 0,
            gid: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            dev_major: major,
            dev_minor: minor,
            sealed: false,
        }
    }
}

pub type VfsResult<T> = Result<T, VfsError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    NotFound,
    NotDirectory,
    NotFile,
    IsDirectory,
    PermissionDenied,
    /// The mount, the filesystem or the device refuses every mutation.
    ReadOnly,
    NoSpace,
    IoError,
    InvalidPath,
    AlreadyExists,
    NotEmpty,
    CrossDevice,
    NotSupported,
    TooManyLinks,
    NameTooLong,
    InvalidArgument,
    BadFileDescriptor,
    Busy,
    /// The calling task was aborted while waiting for the filesystem.
    Interrupted,
}

impl VfsError {
    pub fn to_errno(self) -> slopos_abi::Errno {
        use slopos_abi::Errno;
        match self {
            Self::NotFound => Errno::ENOENT,
            Self::NotDirectory => Errno::ENOTDIR,
            Self::NotFile => Errno::EINVAL,
            Self::IsDirectory => Errno::EISDIR,
            Self::PermissionDenied => Errno::EACCES,
            Self::ReadOnly => Errno::EROFS,
            Self::NoSpace => Errno::ENOSPC,
            Self::IoError => Errno::EIO,
            Self::InvalidPath => Errno::EINVAL,
            Self::AlreadyExists => Errno::EEXIST,
            Self::NotEmpty => Errno::ENOTEMPTY,
            Self::CrossDevice => Errno::EINVAL,
            Self::NotSupported => Errno::EOPNOTSUPP,
            Self::TooManyLinks => Errno::EINVAL,
            Self::NameTooLong => Errno::ENAMETOOLONG,
            Self::InvalidArgument => Errno::EINVAL,
            Self::BadFileDescriptor => Errno::EBADF,
            Self::Busy => Errno::EBUSY,
            Self::Interrupted => Errno::EINTR,
        }
    }
}

/// A filesystem implementation. Operations are inode-based; path resolution is
/// handled by the VFS layer above.
pub trait FileSystem: Send + Sync {
    fn name(&self) -> &'static str;

    fn root_inode(&self) -> InodeId;

    /// Look up a child of `parent`; `name` carries no path separators.
    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId>;

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat>;

    /// `buf` is always kernel memory: the [`FileOps`] layer above stages
    /// user-space I/O, so filesystem code never touches a user address. The
    /// count read may be short at EOF.
    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize>;

    /// `buf` is always kernel memory: the [`FileOps`] layer above stages
    /// user-space I/O, so filesystem code never touches a user address.
    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize>;

    /// Create an entry under `parent`. `file_type` is `Regular` or `Directory`.
    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId>;

    /// Remove an entry from a directory; a directory must be empty.
    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()>;

    /// Iterate directory entries from `offset`, stopping when the callback
    /// returns false; answers the number of entries visited.
    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize>;

    /// Truncate or extend a file; an extension zero-fills on the filesystems
    /// that support one.
    fn truncate(&self, inode: InodeId, size: u64) -> VfsResult<()> {
        let _ = (inode, size);
        Err(VfsError::NotSupported)
    }

    /// Rename/move an entry within the same filesystem.
    ///
    /// # Errors
    /// * `NotFound` - Source entry doesn't exist
    /// * `NotDirectory` - Parent is not a directory
    /// * `AlreadyExists` - Destination name already exists (overwrite not supported)
    /// * `NotSupported` - Filesystem doesn't support rename
    fn rename(
        &self,
        old_parent: InodeId,
        old_name: &[u8],
        new_parent: InodeId,
        new_name: &[u8],
    ) -> VfsResult<()> {
        let _ = (old_parent, old_name, new_parent, new_name);
        Err(VfsError::NotSupported)
    }

    fn readlink(&self, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
        let _ = (inode, buf);
        Err(VfsError::NotSupported)
    }

    fn symlink(&self, parent: InodeId, name: &[u8], target: &[u8]) -> VfsResult<InodeId> {
        let _ = (parent, name, target);
        Err(VfsError::NotSupported)
    }

    /// Seal an inode against every future mutation. One-way: a binary's
    /// contents must not change under a privilege grant keyed on its path.
    /// Defaults to a refusal rather than a no-op, so a filesystem that cannot
    /// store the bit says so instead of leaving the caller falsely reassured.
    fn set_sealed(&self, inode: InodeId) -> VfsResult<()> {
        let _ = inode;
        Err(VfsError::NotSupported)
    }

    /// The initramfs loader uses this to restore the executable bit `create`
    /// dropped (it defaults regular files to `0o644`). Defaults to a no-op for
    /// filesystems that carry no mutable mode bits.
    fn set_mode(&self, inode: InodeId, mode: u16) -> VfsResult<()> {
        let _ = (inode, mode);
        Ok(())
    }

    /// Sync metadata and data to backing store; a no-op for in-memory
    /// filesystems.
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
}
