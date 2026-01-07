//! Filesystem ABI types shared between kernel and userland.

/// Maximum path length for filesystem operations
pub const USER_PATH_MAX: usize = 256;

/// Maximum number of directory entries returned in a single list operation
pub const USER_FS_MAX_ENTRIES: u32 = 64;

/// Filesystem entry type constants
pub const FS_TYPE_FILE: u8 = 0;
pub const FS_TYPE_DIRECTORY: u8 = 1;
pub const FS_TYPE_UNKNOWN: u8 = 0xFF;

/// File open flags
pub const USER_FS_OPEN_READ: u32 = 0x1;
pub const USER_FS_OPEN_WRITE: u32 = 0x2;
pub const USER_FS_OPEN_CREAT: u32 = 0x4;
pub const USER_FS_OPEN_APPEND: u32 = 0x8;

/// Filesystem directory entry information.
///
/// Returned by the fs_list syscall for each entry in a directory.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserFsEntry {
    /// Entry name as UTF-8 bytes (null-terminated)
    pub name: [u8; 64],
    /// Entry type (0 = file, 1 = directory)
    pub type_: u8,
    /// Size in bytes (for files)
    pub size: u32,
}

impl UserFsEntry {
    /// Create a zeroed entry
    pub const fn new() -> Self {
        Self {
            name: [0; 64],
            type_: 0,
            size: 0,
        }
    }

    /// Get the name as a string slice (up to null terminator)
    pub fn name_str(&self) -> &str {
        let len = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..len]).unwrap_or("<invalid>")
    }

    /// Check if this is a directory
    pub fn is_directory(&self) -> bool {
        self.type_ == FS_TYPE_DIRECTORY
    }

    /// Check if this is a file
    pub fn is_file(&self) -> bool {
        self.type_ == FS_TYPE_FILE
    }
}

impl Default for UserFsEntry {
    fn default() -> Self {
        Self::new()
    }
}

/// Filesystem stat information.
///
/// Returned by the fs_stat syscall.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserFsStat {
    /// Entry type (0 = file, 1 = directory)
    pub type_: u8,
    /// Size in bytes
    pub size: u32,
}

impl UserFsStat {
    /// Check if this is a directory
    pub fn is_directory(&self) -> bool {
        self.type_ == FS_TYPE_DIRECTORY
    }

    /// Check if this is a file
    pub fn is_file(&self) -> bool {
        self.type_ == FS_TYPE_FILE
    }
}

/// Filesystem list operation buffer.
///
/// Used by the fs_list syscall to return directory entries.
///
/// Note: Contains a raw pointer, so not Send/Sync by default.
/// Callers must ensure proper synchronization.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserFsList {
    /// Pointer to entry buffer (provided by caller)
    pub entries: *mut UserFsEntry,
    /// Maximum number of entries the buffer can hold
    pub max_entries: u32,
    /// Actual number of entries returned
    pub count: u32,
}

impl Default for UserFsList {
    fn default() -> Self {
        Self {
            entries: core::ptr::null_mut(),
            max_entries: 0,
            count: 0,
        }
    }
}
