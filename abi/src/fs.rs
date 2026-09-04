//! Filesystem ABI types shared between kernel and userland.

pub const USER_PATH_MAX: usize = 256;

pub const USER_FS_MAX_ENTRIES: u32 = 64;

pub const FS_TYPE_FILE: u8 = 0;
pub const FS_TYPE_DIRECTORY: u8 = 1;
pub const FS_TYPE_CHARDEV: u8 = 2;
pub const FS_TYPE_UNKNOWN: u8 = 0xFF;

/// POSIX file open flags (access mode in low 2 bits, modifiers above).
pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_ACCMODE: u32 = 3;
pub const O_CREAT: u32 = 0x40;
pub const O_EXCL: u32 = 0x80;
pub const O_TRUNC: u32 = 0x200;
pub const O_APPEND: u32 = 0x400;
/// Every write commits the file's data before returning. The values are the
/// Linux x86-64 ones so a port needs no translation table; note that there
/// `O_SYNC` subsumes `O_DSYNC`, which is why the two are not disjoint bits.
pub const O_DSYNC: u32 = 0x1000;
pub const O_SYNC: u32 = 0x101000;

/// Directory entry returned by the fs_list syscall.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserFsEntry {
    /// Entry name as UTF-8 bytes (null-terminated)
    pub name: [u8; 64],
    pub type_: u8,
    pub size: u32,
}

impl UserFsEntry {
    pub const fn new() -> Self {
        Self {
            name: [0; 64],
            type_: 0,
            size: 0,
        }
    }

    pub fn name_str(&self) -> &str {
        let len = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..len]).unwrap_or("<invalid>")
    }

    pub fn is_directory(&self) -> bool {
        self.type_ == FS_TYPE_DIRECTORY
    }

    pub fn is_file(&self) -> bool {
        self.type_ == FS_TYPE_FILE
    }
}

impl Default for UserFsEntry {
    fn default() -> Self {
        Self::new()
    }
}

/// Stat information returned by the fs_stat syscall.
///
/// `_pad` is named rather than implicit: `copy_to_user` copies
/// `size_of::<Self>()` bytes, and a hole between `type_` and `size` would
/// carry three uninitialized bytes of the calling task's kernel stack to
/// userland on every call.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserFsStat {
    pub type_: u8,
    pub _pad: [u8; 3],
    pub size: u32,
}

const _: () = assert!(
    core::mem::size_of::<UserFsStat>() == 8,
    "UserFsStat must carry no implicit padding"
);

impl UserFsStat {
    pub fn is_directory(&self) -> bool {
        self.type_ == FS_TYPE_DIRECTORY
    }

    pub fn is_file(&self) -> bool {
        self.type_ == FS_TYPE_FILE
    }
}

/// Caller-provided entry buffer for the fs_list syscall.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserFsList {
    pub entries: *mut UserFsEntry,
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
