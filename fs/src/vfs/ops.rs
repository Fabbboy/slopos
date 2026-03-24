use crate::vfs::mount::with_mount_table;
use crate::vfs::path::{resolve_parent, resolve_path};
use crate::vfs::traits::{FileType, InodeId, VfsError, VfsResult};
use slopos_abi::fs::{FS_TYPE_DIRECTORY, FS_TYPE_FILE, FS_TYPE_UNKNOWN, UserFsEntry};

pub struct VfsHandle {
    pub inode: InodeId,
    pub fs: &'static dyn crate::vfs::FileSystem,
}

impl VfsHandle {
    pub fn read(&self, offset: u64, buf: &mut dyn slopos_abi::io::IoBuf) -> VfsResult<usize> {
        self.fs.read(self.inode, offset, buf)
    }

    pub fn write(&self, offset: u64, buf: &mut dyn slopos_abi::io::IoBuf) -> VfsResult<usize> {
        self.fs.write(self.inode, offset, buf)
    }

    pub fn size(&self) -> VfsResult<u64> {
        let stat = self.fs.stat(self.inode)?;
        Ok(stat.size)
    }

    pub fn is_directory(&self) -> VfsResult<bool> {
        let stat = self.fs.stat(self.inode)?;
        Ok(stat.file_type == FileType::Directory)
    }
}

/// Open flags for [`vfs_open`].
pub struct VfsOpenFlags {
    pub create: bool,
    pub exclusive: bool,
    pub truncate: bool,
}

impl VfsOpenFlags {
    pub const fn read_only() -> Self {
        Self {
            create: false,
            exclusive: false,
            truncate: false,
        }
    }

    pub const fn create_only() -> Self {
        Self {
            create: true,
            exclusive: false,
            truncate: false,
        }
    }
}

pub fn vfs_open(path: &[u8], create: bool) -> VfsResult<VfsHandle> {
    vfs_open_flags(
        path,
        VfsOpenFlags {
            create,
            exclusive: false,
            truncate: false,
        },
    )
}

pub fn vfs_open_flags(path: &[u8], flags: VfsOpenFlags) -> VfsResult<VfsHandle> {
    match resolve_path(path) {
        Ok(resolved) => {
            // O_EXCL + O_CREAT: file must not exist.
            if flags.create && flags.exclusive {
                return Err(VfsError::AlreadyExists);
            }
            let stat = resolved.fs.stat(resolved.inode)?;
            if stat.file_type == FileType::Directory {
                return Err(VfsError::IsDirectory);
            }
            // O_TRUNC: truncate existing file to zero length.
            if flags.truncate {
                let _ = resolved.fs.truncate(resolved.inode, 0);
            }
            Ok(VfsHandle {
                inode: resolved.inode,
                fs: resolved.fs,
            })
        }
        Err(VfsError::NotFound) if flags.create => {
            let (parent, name) = resolve_parent(path)?;
            let new_inode = parent.fs.create(parent.inode, name, FileType::Regular)?;
            Ok(VfsHandle {
                inode: new_inode,
                fs: parent.fs,
            })
        }
        Err(e) => Err(e),
    }
}

pub fn vfs_stat(path: &[u8]) -> VfsResult<(u8, u32)> {
    let resolved = resolve_path(path)?;
    let stat = resolved.fs.stat(resolved.inode)?;

    let kind = match stat.file_type {
        FileType::Directory => FS_TYPE_DIRECTORY,
        FileType::Regular => FS_TYPE_FILE,
        _ => FS_TYPE_UNKNOWN,
    };

    Ok((kind, stat.size as u32))
}

pub fn vfs_mkdir(path: &[u8]) -> VfsResult<()> {
    let (parent, name) = resolve_parent(path)?;
    parent.fs.create(parent.inode, name, FileType::Directory)?;
    Ok(())
}

pub fn vfs_unlink(path: &[u8]) -> VfsResult<()> {
    let (parent, name) = resolve_parent(path)?;
    parent.fs.unlink(parent.inode, name)
}

pub fn vfs_rename(old_path: &[u8], new_path: &[u8]) -> VfsResult<()> {
    let (old_parent, old_name) = resolve_parent(old_path)?;
    let (new_parent, new_name) = resolve_parent(new_path)?;

    if !core::ptr::eq(old_parent.fs, new_parent.fs) {
        return Err(VfsError::CrossDevice);
    }

    old_parent
        .fs
        .rename(old_parent.inode, old_name, new_parent.inode, new_name)
}

pub fn vfs_list(path: &[u8], entries: &mut [UserFsEntry]) -> VfsResult<usize> {
    let resolved = resolve_path(path)?;
    let stat = resolved.fs.stat(resolved.inode)?;

    if stat.file_type != FileType::Directory {
        return Err(VfsError::NotDirectory);
    }

    let mut count = 0usize;
    let max = entries.len();
    let mut inodes = [0u64; 64];

    resolved
        .fs
        .readdir(resolved.inode, 0, &mut |name, inode, file_type| {
            if count >= max || count >= 64 {
                return false;
            }

            let entry = &mut entries[count];
            *entry = UserFsEntry::new();

            let nlen = name.len().min(entry.name.len() - 1);
            entry.name[..nlen].copy_from_slice(&name[..nlen]);
            entry.name[nlen] = 0;

            entry.type_ = match file_type {
                FileType::Directory => FS_TYPE_DIRECTORY,
                FileType::Regular => FS_TYPE_FILE,
                _ => FS_TYPE_UNKNOWN,
            };

            inodes[count] = inode;
            count += 1;
            true
        })?;

    for i in 0..count {
        if let Ok(child_stat) = resolved.fs.stat(inodes[i]) {
            entries[i].size = child_stat.size as u32;
        }
    }

    // Overlay child mount points (Linux VFS behaviour: mount points appear
    // as directory entries in the parent listing even when the underlying
    // filesystem has no matching entry).
    with_mount_table(|mt| {
        mt.for_each_child_mount(path, &mut |child_name| {
            if count >= max {
                return false;
            }

            // If an entry with this name already exists, just ensure it
            // shows as a directory (mount points are always directories).
            for i in 0..count {
                let elen = entries[i]
                    .name
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(entries[i].name.len());
                if elen == child_name.len() && &entries[i].name[..elen] == child_name {
                    entries[i].type_ = FS_TYPE_DIRECTORY;
                    return true;
                }
            }

            let entry = &mut entries[count];
            *entry = UserFsEntry::new();
            let nlen = child_name.len().min(entry.name.len() - 1);
            entry.name[..nlen].copy_from_slice(&child_name[..nlen]);
            entry.name[nlen] = 0;
            entry.type_ = FS_TYPE_DIRECTORY;
            entry.size = 0;
            count += 1;
            true
        });
    });

    Ok(count)
}
