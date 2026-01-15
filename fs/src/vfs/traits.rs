//! VFS trait definitions for the SlopOS Virtual File System layer.
//!
//! This module defines the core abstractions that all filesystem implementations
//! must adhere to. The design is inspired by Linux VFS but simplified for SlopOS.

/// Unique identifier for an inode within a filesystem.
/// Each filesystem maintains its own inode number space.
pub type InodeId = u64;

/// File type enumeration matching Unix file types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileType {
    /// Regular file
    Regular = 1,
    /// Directory
    Directory = 2,
    /// Character device (e.g., /dev/null)
    CharDevice = 3,
    /// Block device
    BlockDevice = 4,
    /// Symbolic link
    Symlink = 5,
    /// Named pipe (FIFO)
    Pipe = 6,
    /// Unix domain socket
    Socket = 7,
}

/// Metadata about a file or directory.
/// Returned by stat operations.
#[derive(Debug, Clone)]
pub struct FileStat {
    /// Inode number within the filesystem
    pub inode: InodeId,
    /// Type of file (regular, directory, device, etc.)
    pub file_type: FileType,
    /// Size in bytes (0 for directories, devices)
    pub size: u64,
    /// Unix permission bits (rwxrwxrwx)
    pub mode: u16,
    /// Number of hard links
    pub nlink: u32,
    /// Owner user ID
    pub uid: u32,
    /// Owner group ID
    pub gid: u32,
    /// Last access time (Unix timestamp)
    pub atime: u64,
    /// Last modification time (Unix timestamp)
    pub mtime: u64,
    /// Last status change time (Unix timestamp)
    pub ctime: u64,
    /// Major device number (for device files)
    pub dev_major: u32,
    /// Minor device number (for device files)
    pub dev_minor: u32,
}

impl FileStat {
    /// Create a new FileStat with default values for a regular file.
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
        }
    }

    /// Create a new FileStat with default values for a directory.
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
        }
    }

    /// Create a new FileStat for a character device.
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
        }
    }
}

/// Result type for VFS operations.
pub type VfsResult<T> = Result<T, VfsError>;

/// Errors that can occur during VFS operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    /// File or directory not found (ENOENT)
    NotFound,
    /// Path component is not a directory (ENOTDIR)
    NotDirectory,
    /// Expected a file but got something else
    NotFile,
    /// Operation not permitted on a directory (EISDIR)
    IsDirectory,
    /// Permission denied (EACCES)
    PermissionDenied,
    /// Filesystem is read-only (EROFS)
    ReadOnly,
    /// No space left on device (ENOSPC)
    NoSpace,
    /// I/O error (EIO)
    IoError,
    /// Invalid path format
    InvalidPath,
    /// File or directory already exists (EEXIST)
    AlreadyExists,
    /// Directory is not empty (ENOTEMPTY)
    NotEmpty,
    /// Cross-device link not permitted (EXDEV)
    CrossDevice,
    /// Operation not supported (ENOTSUP)
    NotSupported,
    /// Too many symbolic links (ELOOP)
    TooManyLinks,
    /// Filename too long (ENAMETOOLONG)
    NameTooLong,
    /// Invalid argument (EINVAL)
    InvalidArgument,
    /// Bad file descriptor
    BadFileDescriptor,
    /// Resource busy (EBUSY)
    Busy,
}

/// A filesystem implementation.
///
/// All filesystem types (ext2, ramfs, devfs, etc.) implement this trait.
/// Operations are inode-based internally, with path resolution handled
/// by the VFS layer above.
pub trait FileSystem: Send + Sync {
    /// Get the name of this filesystem type (e.g., "ext2", "ramfs", "devfs").
    fn name(&self) -> &'static str;

    /// Get the root inode of this filesystem.
    /// This is the entry point for all path traversal within this mount.
    fn root_inode(&self) -> InodeId;

    /// Look up a child entry in a directory by name.
    ///
    /// # Arguments
    /// * `parent` - Inode of the parent directory
    /// * `name` - Name of the entry to look up (without path separators)
    ///
    /// # Returns
    /// The inode of the found entry, or `VfsError::NotFound`.
    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId>;

    /// Get metadata (stat) for an inode.
    fn stat(&self, inode: InodeId) -> VfsResult<FileStat>;

    /// Read data from a file.
    ///
    /// # Arguments
    /// * `inode` - The file's inode
    /// * `offset` - Byte offset to start reading from
    /// * `buf` - Buffer to read into
    ///
    /// # Returns
    /// Number of bytes actually read (may be less than buffer size at EOF).
    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize>;

    /// Write data to a file.
    ///
    /// # Arguments
    /// * `inode` - The file's inode
    /// * `offset` - Byte offset to start writing at
    /// * `buf` - Data to write
    ///
    /// # Returns
    /// Number of bytes actually written.
    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize>;

    /// Create a new file or directory in a parent directory.
    ///
    /// # Arguments
    /// * `parent` - Inode of the parent directory
    /// * `name` - Name for the new entry
    /// * `file_type` - Type of entry to create (Regular or Directory)
    ///
    /// # Returns
    /// The inode of the newly created entry.
    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId>;

    /// Remove an entry from a directory.
    ///
    /// For directories, this is rmdir (must be empty).
    /// For files, this is unlink.
    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()>;

    /// Iterate over directory entries.
    ///
    /// # Arguments
    /// * `inode` - Directory inode to read
    /// * `offset` - Entry offset to start from (0 for beginning)
    /// * `callback` - Called for each entry; return false to stop iteration
    ///
    /// # Returns
    /// The number of entries visited.
    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize>;

    /// Truncate a file to a specified length.
    ///
    /// If the file is larger, data is discarded.
    /// If smaller, the file is extended with zeros (on supporting filesystems).
    fn truncate(&self, inode: InodeId, size: u64) -> VfsResult<()> {
        let _ = (inode, size);
        Err(VfsError::NotSupported)
    }

    /// Sync filesystem metadata and data to backing store.
    fn sync(&self) -> VfsResult<()> {
        // Default: no-op for in-memory filesystems
        Ok(())
    }
}
