use crate::vfs::path::{resolve_parent, resolve_path};
use crate::vfs::traits::{FileType, InodeId, VfsError, VfsResult};
use slopos_abi::fs::{FS_TYPE_DIRECTORY, FS_TYPE_FILE, FS_TYPE_UNKNOWN, UserFsEntry};

pub struct VfsHandle {
    pub inode: InodeId,
    pub fs: &'static dyn crate::vfs::FileSystem,
}

impl VfsHandle {
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.fs.read(self.inode, offset, buf)
    }

    pub fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
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

pub fn vfs_open(path: &[u8], create: bool) -> VfsResult<VfsHandle> {
    match resolve_path(path) {
        Ok(resolved) => {
            let stat = resolved.fs.stat(resolved.inode)?;
            if stat.file_type == FileType::Directory {
                return Err(VfsError::IsDirectory);
            }
            Ok(VfsHandle {
                inode: resolved.inode,
                fs: resolved.fs,
            })
        }
        Err(VfsError::NotFound) if create => {
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

    Ok(count)
}
