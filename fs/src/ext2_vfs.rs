use crate::blockdev::BlockDevice;
use crate::ext2::{Ext2Error, Ext2Fs, Ext2Inode, Ext2Superblock};
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
use slopos_ostd::KBox;
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_RESOURCE, PreemptMutex};

const EXT2_ROOT_INODE: u32 = 2;

// ============================================================================
// Cached ext2 state — superblock is read once at mount, not on every VFS call
// ============================================================================

struct CachedExt2 {
    /// The filesystem's sole writable handle to its backing block device —
    /// an owned capability (e.g. a virtio-blk `BlockWriteToken`) boxed behind
    /// `dyn BlockDevice`. Held for the kernel's lifetime, so no other code can
    /// acquire a second writer to this device (Layer 1: ownership = exclusion).
    device: KBox<dyn BlockDevice + Send + Sync>,
    superblock: Ext2Superblock,
    block_size: u32,
    inode_size: u16,
}

static CACHED_EXT2: PreemptMutex<Option<CachedExt2>> = PreemptMutex::new(None, LOCK_LEVEL_RESOURCE);
static EXT2_VFS_INIT: InitFlag = InitFlag::new();

pub struct StaticExt2Vfs;

impl StaticExt2Vfs {
    fn with_fs<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R> {
        if !EXT2_VFS_INIT.is_set() {
            return Err(VfsError::IoError);
        }
        let mut guard = CACHED_EXT2.lock();
        let cached = guard.as_mut().ok_or(VfsError::IoError)?;
        let mut fs = Ext2Fs::from_parts(
            &*cached.device,
            cached.superblock,
            cached.block_size,
            cached.inode_size,
        )
        .map_err(ext2_error_to_vfs)?;
        let result = f(&mut fs).map_err(ext2_error_to_vfs);
        // Persist any superblock mutations (free counts change on alloc/dealloc)
        cached.superblock = fs.superblock();
        result
    }
}

trait Ext2VfsBackend {
    fn with_ext2<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R>;
}

impl Ext2VfsBackend for StaticExt2Vfs {
    fn with_ext2<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R> {
        self.with_fs(f)
    }
}

impl<T: Ext2VfsBackend + Send + Sync> FileSystem for T {
    fn name(&self) -> &'static str {
        "ext2"
    }

    fn root_inode(&self) -> InodeId {
        EXT2_ROOT_INODE as InodeId
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        self.with_ext2(|fs| {
            let parent_inode = fs.read_inode(parent as u32)?;
            if !parent_inode.is_directory() {
                return Err(Ext2Error::NotDirectory);
            }
            let mut found: Option<u32> = None;
            fs.for_each_dir_entry(parent as u32, |entry| {
                if entry.name == name {
                    found = Some(entry.inode.raw());
                    false
                } else {
                    true
                }
            })?;
            found.map(|i| i as InodeId).ok_or(Ext2Error::PathNotFound)
        })
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        self.with_ext2(|fs| {
            let ext2_inode = fs.read_inode(inode as u32)?;
            Ok(FileStat {
                inode,
                file_type: inode_to_file_type(&ext2_inode),
                size: ext2_inode.size as u64,
                mode: ext2_inode.mode,
                nlink: ext2_inode.links_count as u32,
                uid: ext2_inode.uid as u32,
                gid: ext2_inode.gid as u32,
                atime: ext2_inode.atime as u64,
                mtime: ext2_inode.mtime as u64,
                ctime: ext2_inode.ctime as u64,
                dev_major: 0,
                dev_minor: 0,
            })
        })
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.read_file(inode as u32, offset as u32, buf))
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.write_file(inode as u32, offset as u32, buf))
    }

    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        self.with_ext2(|fs| {
            let inode = match file_type {
                FileType::Directory => fs.create_directory(parent as u32, name)?,
                FileType::Regular => fs.create_file(parent as u32, name)?,
                _ => return Err(Ext2Error::InvalidInode),
            };
            Ok(inode as InodeId)
        })
    }

    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        self.with_ext2(|fs| fs.unlink_entry(parent as u32, name))
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        self.with_ext2(|fs| {
            let ext2_inode = fs.read_inode(inode as u32)?;
            if !ext2_inode.is_directory() {
                return Err(Ext2Error::NotDirectory);
            }
            let mut count = 0usize;
            let mut current = 0usize;
            fs.for_each_dir_entry(inode as u32, |entry| {
                if current < offset {
                    current += 1;
                    return true;
                }
                let ft = ext2_file_type_to_vfs(entry.file_type);
                let cont = callback(entry.name, entry.inode.raw() as InodeId, ft);
                count += 1;
                current += 1;
                cont
            })?;
            Ok(count)
        })
    }

    fn truncate(&self, _inode: InodeId, _size: u64) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
}

pub static EXT2_VFS_STATIC: StaticExt2Vfs = StaticExt2Vfs;

pub fn ext2_vfs_init_with_device(device: KBox<dyn BlockDevice + Send + Sync>) -> VfsResult<()> {
    if !EXT2_VFS_INIT.init_once() {
        return Ok(());
    }

    // Validate the filesystem by doing a full init against the owned device.
    let fs = Ext2Fs::init_internal(&*device).map_err(ext2_error_to_vfs)?;
    let superblock = fs.superblock();
    let block_size = fs.block_size();
    let inode_size = superblock.inode_size;

    let mut guard = CACHED_EXT2.lock();
    *guard = Some(CachedExt2 {
        device,
        superblock,
        block_size,
        inode_size: if inode_size == 0 { 128 } else { inode_size },
    });

    Ok(())
}

pub fn ext2_vfs_is_initialized() -> bool {
    EXT2_VFS_INIT.is_set()
}

// ============================================================================
// Helpers
// ============================================================================

fn ext2_error_to_vfs(e: Ext2Error) -> VfsError {
    match e {
        Ext2Error::InvalidSuperblock => VfsError::IoError,
        Ext2Error::UnsupportedBlockSize => VfsError::IoError,
        Ext2Error::InvalidInode => VfsError::NotFound,
        Ext2Error::InvalidBlock => VfsError::IoError,
        Ext2Error::UnsupportedIndirection => VfsError::NotSupported,
        Ext2Error::DeviceError => VfsError::IoError,
        Ext2Error::DirectoryFormat => VfsError::IoError,
        Ext2Error::NotDirectory => VfsError::NotDirectory,
        Ext2Error::NotFile => VfsError::NotFile,
        Ext2Error::PathNotFound => VfsError::NotFound,
        Ext2Error::NoSpace => VfsError::NoSpace,
        Ext2Error::NameTooLong => VfsError::NameTooLong,
        Ext2Error::AlreadyExists => VfsError::AlreadyExists,
        Ext2Error::NotEmpty => VfsError::NotEmpty,
        Ext2Error::IsDirectory => VfsError::IsDirectory,
        Ext2Error::TooManyLinks => VfsError::TooManyLinks,
        Ext2Error::OutOfMemory => VfsError::IoError,
    }
}

fn inode_to_file_type(inode: &Ext2Inode) -> FileType {
    let mode = inode.mode & 0xF000;
    match mode {
        0x4000 => FileType::Directory,
        0x8000 => FileType::Regular,
        0xA000 => FileType::Symlink,
        0x2000 => FileType::CharDevice,
        0x6000 => FileType::BlockDevice,
        0x1000 => FileType::Pipe,
        0xC000 => FileType::Socket,
        _ => FileType::Regular,
    }
}

fn ext2_file_type_to_vfs(file_type: u8) -> FileType {
    match file_type {
        1 => FileType::Regular,
        2 => FileType::Directory,
        3 => FileType::CharDevice,
        4 => FileType::BlockDevice,
        5 => FileType::Pipe,
        6 => FileType::Socket,
        7 => FileType::Symlink,
        _ => FileType::Regular,
    }
}
