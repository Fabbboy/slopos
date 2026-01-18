use crate::blockdev::{BlockDevice, CallbackBlockDevice, CapacityFn, ReadFn, WriteFn};
use crate::ext2::{Ext2Error, Ext2Fs, Ext2Inode};
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
use slopos_lib::{InitFlag, IrqMutex};

const EXT2_ROOT_INODE: u32 = 2;

// ============================================================================
// Global ext2 VFS adapter using virtio-blk callbacks
// ============================================================================

/// Storage for the global ext2 VFS adapter
struct GlobalExt2Vfs {
    device: Option<CallbackBlockDevice>,
}

impl GlobalExt2Vfs {
    const fn new() -> Self {
        Self { device: None }
    }
}

static GLOBAL_EXT2_VFS: IrqMutex<GlobalExt2Vfs> = IrqMutex::new(GlobalExt2Vfs::new());
static EXT2_VFS_INIT: InitFlag = InitFlag::new();

/// Static wrapper that implements FileSystem by delegating to the global ext2 state.
/// This enables mounting ext2 at "/" through the VFS layer.
pub struct StaticExt2Vfs;

impl StaticExt2Vfs {
    fn with_fs<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R> {
        if !EXT2_VFS_INIT.is_set() {
            return Err(VfsError::IoError);
        }
        let mut guard = GLOBAL_EXT2_VFS.lock();
        let device = guard.device.as_mut().ok_or(VfsError::IoError)?;
        let mut fs = Ext2Fs::init_internal(device).map_err(ext2_error_to_vfs)?;
        f(&mut fs).map_err(ext2_error_to_vfs)
    }
}

impl FileSystem for StaticExt2Vfs {
    fn name(&self) -> &'static str {
        "ext2"
    }

    fn root_inode(&self) -> InodeId {
        EXT2_ROOT_INODE as InodeId
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        self.with_fs(|fs| {
            let parent_inode = fs.read_inode(parent as u32)?;
            if !parent_inode.is_directory() {
                return Err(Ext2Error::NotDirectory);
            }

            let mut found: Option<u32> = None;
            fs.for_each_dir_entry(parent as u32, |entry| {
                if entry.name == name {
                    found = Some(entry.inode);
                    false
                } else {
                    true
                }
            })?;

            found.map(|i| i as InodeId).ok_or(Ext2Error::PathNotFound)
        })
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        self.with_fs(|fs| {
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
        self.with_fs(|fs| fs.read_file(inode as u32, offset as u32, buf))
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.with_fs(|fs| fs.write_file(inode as u32, offset as u32, buf))
    }

    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        self.with_fs(|fs| {
            let inode = match file_type {
                FileType::Directory => fs.create_directory(parent as u32, name)?,
                FileType::Regular => fs.create_file(parent as u32, name)?,
                _ => return Err(Ext2Error::InvalidInode),
            };
            Ok(inode as InodeId)
        })
    }

    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        self.with_fs(|fs| {
            let _target_inode = {
                let mut found: Option<u32> = None;
                fs.for_each_dir_entry(parent as u32, |entry| {
                    if entry.name == name {
                        found = Some(entry.inode);
                        false
                    } else {
                        true
                    }
                })?;
                found.ok_or(Ext2Error::PathNotFound)?
            };

            let mut path_buf = [0u8; 256];
            let mut pos = 0;

            let mut build_path =
                |fs: &mut Ext2Fs, start: u32, name: &[u8]| -> Result<usize, Ext2Error> {
                    let mut components: [([u8; 64], usize); 16] = [([0u8; 64], 0); 16];
                    let mut depth = 0;
                    let mut current = start;

                    while current != EXT2_ROOT_INODE && depth < 16 {
                        let mut parent_found: Option<u32> = None;
                        let mut name_found: Option<([u8; 64], usize)> = None;

                        fs.for_each_dir_entry(current, |entry| {
                            if entry.name == b".." {
                                parent_found = Some(entry.inode);
                            }
                            true
                        })?;

                        let parent = parent_found.ok_or(Ext2Error::PathNotFound)?;

                        fs.for_each_dir_entry(parent, |entry| {
                            if entry.inode == current && entry.name != b"." && entry.name != b".." {
                                let mut buf = [0u8; 64];
                                let len = entry.name.len().min(64);
                                buf[..len].copy_from_slice(&entry.name[..len]);
                                name_found = Some((buf, len));
                                false
                            } else {
                                true
                            }
                        })?;

                        if let Some((buf, len)) = name_found {
                            components[depth] = (buf, len);
                            depth += 1;
                        }

                        current = parent;
                    }

                    path_buf[pos] = b'/';
                    pos += 1;

                    for i in (0..depth).rev() {
                        let (buf, len) = components[i];
                        path_buf[pos..pos + len].copy_from_slice(&buf[..len]);
                        pos += len;
                        path_buf[pos] = b'/';
                        pos += 1;
                    }

                    let name_len = name.len().min(256 - pos);
                    path_buf[pos..pos + name_len].copy_from_slice(&name[..name_len]);
                    pos += name_len;

                    Ok(pos)
                };

            let path_len = build_path(fs, parent as u32, name)?;
            fs.remove_path(&path_buf[..path_len])
        })
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        self.with_fs(|fs| {
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
                let cont = callback(entry.name, entry.inode as InodeId, ft);
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

unsafe impl Send for StaticExt2Vfs {}
unsafe impl Sync for StaticExt2Vfs {}

/// Global static instance for mounting
pub static EXT2_VFS_STATIC: StaticExt2Vfs = StaticExt2Vfs;

/// Initialize the global ext2 VFS adapter with virtio-blk callbacks.
pub fn ext2_vfs_init_with_callbacks(
    read_fn: ReadFn,
    write_fn: WriteFn,
    capacity_fn: CapacityFn,
) -> VfsResult<()> {
    if !EXT2_VFS_INIT.init_once() {
        return Ok(());
    }

    let device = CallbackBlockDevice::new(read_fn, write_fn, capacity_fn);

    // Verify ext2 superblock is valid
    {
        let mut test_device = CallbackBlockDevice::new(read_fn, write_fn, capacity_fn);
        Ext2Fs::init_internal(&mut test_device).map_err(ext2_error_to_vfs)?;
    }

    let mut guard = GLOBAL_EXT2_VFS.lock();
    guard.device = Some(device);

    Ok(())
}

pub fn ext2_vfs_is_initialized() -> bool {
    EXT2_VFS_INIT.is_set()
}

// ============================================================================
// Helper functions
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
    }
}

fn inode_to_file_type(inode: &Ext2Inode) -> FileType {
    if inode.is_directory() {
        FileType::Directory
    } else if inode.is_regular_file() {
        FileType::Regular
    } else {
        FileType::Regular
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

pub struct Ext2VfsAdapter<D: BlockDevice + Send> {
    device: IrqMutex<D>,
}

impl<D: BlockDevice + Send> Ext2VfsAdapter<D> {
    pub fn new(device: D) -> VfsResult<Self> {
        let adapter = Self {
            device: IrqMutex::new(device),
        };

        {
            let mut dev = adapter.device.lock();
            Ext2Fs::init_internal(&mut *dev).map_err(ext2_error_to_vfs)?;
        }

        Ok(adapter)
    }

    fn with_fs<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R> {
        let mut dev = self.device.lock();
        let mut fs = Ext2Fs::init_internal(&mut *dev).map_err(ext2_error_to_vfs)?;
        f(&mut fs).map_err(ext2_error_to_vfs)
    }
}

impl<D: BlockDevice + Send + 'static> FileSystem for Ext2VfsAdapter<D> {
    fn name(&self) -> &'static str {
        "ext2"
    }

    fn root_inode(&self) -> InodeId {
        EXT2_ROOT_INODE as InodeId
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        self.with_fs(|fs| {
            let parent_inode = fs.read_inode(parent as u32)?;
            if !parent_inode.is_directory() {
                return Err(Ext2Error::NotDirectory);
            }

            let mut found: Option<u32> = None;
            fs.for_each_dir_entry(parent as u32, |entry| {
                if entry.name == name {
                    found = Some(entry.inode);
                    false
                } else {
                    true
                }
            })?;

            found.map(|i| i as InodeId).ok_or(Ext2Error::PathNotFound)
        })
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        self.with_fs(|fs| {
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
        self.with_fs(|fs| fs.read_file(inode as u32, offset as u32, buf))
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.with_fs(|fs| fs.write_file(inode as u32, offset as u32, buf))
    }

    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        self.with_fs(|fs| {
            let inode = match file_type {
                FileType::Directory => fs.create_directory(parent as u32, name)?,
                FileType::Regular => fs.create_file(parent as u32, name)?,
                _ => return Err(Ext2Error::InvalidInode),
            };
            Ok(inode as InodeId)
        })
    }

    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        self.with_fs(|fs| {
            let _target_inode = {
                let mut found: Option<u32> = None;
                fs.for_each_dir_entry(parent as u32, |entry| {
                    if entry.name == name {
                        found = Some(entry.inode);
                        false
                    } else {
                        true
                    }
                })?;
                found.ok_or(Ext2Error::PathNotFound)?
            };

            let mut path_buf = [0u8; 256];
            let mut pos = 0;

            let mut build_path =
                |fs: &mut Ext2Fs, start: u32, name: &[u8]| -> Result<usize, Ext2Error> {
                    let mut components: [([u8; 64], usize); 16] = [([0u8; 64], 0); 16];
                    let mut depth = 0;
                    let mut current = start;

                    while current != EXT2_ROOT_INODE && depth < 16 {
                        let mut parent_found: Option<u32> = None;
                        let mut name_found: Option<([u8; 64], usize)> = None;

                        fs.for_each_dir_entry(current, |entry| {
                            if entry.name == b".." {
                                parent_found = Some(entry.inode);
                            }
                            true
                        })?;

                        let parent = parent_found.ok_or(Ext2Error::PathNotFound)?;

                        fs.for_each_dir_entry(parent, |entry| {
                            if entry.inode == current && entry.name != b"." && entry.name != b".." {
                                let mut buf = [0u8; 64];
                                let len = entry.name.len().min(64);
                                buf[..len].copy_from_slice(&entry.name[..len]);
                                name_found = Some((buf, len));
                                false
                            } else {
                                true
                            }
                        })?;

                        if let Some((buf, len)) = name_found {
                            components[depth] = (buf, len);
                            depth += 1;
                        }

                        current = parent;
                    }

                    path_buf[pos] = b'/';
                    pos += 1;

                    for i in (0..depth).rev() {
                        let (buf, len) = components[i];
                        path_buf[pos..pos + len].copy_from_slice(&buf[..len]);
                        pos += len;
                        path_buf[pos] = b'/';
                        pos += 1;
                    }

                    let name_len = name.len().min(256 - pos);
                    path_buf[pos..pos + name_len].copy_from_slice(&name[..name_len]);
                    pos += name_len;

                    Ok(pos)
                };

            let path_len = build_path(fs, parent as u32, name)?;
            fs.remove_path(&path_buf[..path_len])
        })
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        self.with_fs(|fs| {
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
                let cont = callback(entry.name, entry.inode as InodeId, ft);
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

unsafe impl<D: BlockDevice + Send> Send for Ext2VfsAdapter<D> {}
unsafe impl<D: BlockDevice + Send> Sync for Ext2VfsAdapter<D> {}
