use crate::vfs::mount::{MAX_MOUNTS, with_mount_table};
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

pub struct VfsOpenFlags {
    pub create: bool,
    pub exclusive: bool,
    pub truncate: bool,
    pub writable: bool,
}

impl VfsOpenFlags {
    pub const fn read_only() -> Self {
        Self {
            create: false,
            exclusive: false,
            truncate: false,
            writable: false,
        }
    }

    pub const fn create_only() -> Self {
        Self {
            create: true,
            exclusive: false,
            truncate: false,
            writable: true,
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
            writable: create,
        },
    )
}

pub fn vfs_open_flags(path: &[u8], flags: VfsOpenFlags) -> VfsResult<VfsHandle> {
    match resolve_path(path) {
        Ok(resolved) => {
            if flags.create && flags.exclusive {
                return Err(VfsError::AlreadyExists);
            }
            let stat = resolved.fs.stat(resolved.inode)?;
            if stat.file_type == FileType::Directory {
                return Err(VfsError::IsDirectory);
            }
            // Refused at open, not at the first write: a descriptor obtained
            // before the check is a descriptor that outlives it.
            if stat.sealed && flags.writable {
                return Err(VfsError::PermissionDenied);
            }
            if flags.truncate && flags.writable && stat.file_type == FileType::Regular {
                resolved.fs.truncate(resolved.inode, 0)?;
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

pub fn vfs_set_mode(path: &[u8], mode: u16) -> VfsResult<()> {
    let resolved = resolve_path(path)?;
    resolved.fs.set_mode(resolved.inode, mode)
}

/// Seal `path` against every future mutation. One-way and un-clearable.
pub fn vfs_set_sealed(path: &[u8]) -> VfsResult<()> {
    let resolved = resolve_path(path)?;
    resolved.fs.set_sealed(resolved.inode)
}

/// Whether `path` names a sealed inode. A path that resolves to nothing is not
/// sealed — the caller's own lookup reports that.
fn path_is_sealed(path: &[u8]) -> bool {
    resolve_path(path)
        .and_then(|r| r.fs.stat(r.inode))
        .map(|s| s.sealed)
        .unwrap_or(false)
}

pub fn vfs_unlink(path: &[u8]) -> VfsResult<()> {
    if path_is_sealed(path) {
        return Err(VfsError::PermissionDenied);
    }
    let (parent, name) = resolve_parent(path)?;
    parent.fs.unlink(parent.inode, name)
}

pub fn vfs_rename(old_path: &[u8], new_path: &[u8]) -> VfsResult<()> {
    // Both ends: renaming a sealed file moves it out from under the path its
    // privilege is keyed on, and renaming over one replaces it just as a write
    // would.
    if path_is_sealed(old_path) || path_is_sealed(new_path) {
        return Err(VfsError::PermissionDenied);
    }
    let (old_parent, old_name) = resolve_parent(old_path)?;
    let (new_parent, new_name) = resolve_parent(new_path)?;

    if !core::ptr::eq(old_parent.fs, new_parent.fs) {
        return Err(VfsError::CrossDevice);
    }

    old_parent
        .fs
        .rename(old_parent.inode, old_name, new_parent.inode, new_name)
}

/// Commits every mount, returning the first error. Snapshots the table and
/// drops its `IrqRwLock` before the first `sync`: ext2's takes a sleeping
/// mutex, which must not be acquired under it.
pub fn vfs_sync_all() -> VfsResult<()> {
    let mut snapshot: [Option<&'static dyn crate::vfs::FileSystem>; MAX_MOUNTS] =
        [None; MAX_MOUNTS];
    let mut n = 0usize;
    with_mount_table(|table| {
        table.for_each_mount(&mut |fs| {
            if n < snapshot.len() {
                snapshot[n] = Some(fs);
                n += 1;
            }
        });
    });

    let mut first_err = None;
    for fs in snapshot.iter().take(n).flatten() {
        if let Err(e) = fs.sync() {
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
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

    // Mount points appear as directory entries in the parent listing even when
    // the underlying filesystem has no matching entry (Linux VFS behaviour).
    with_mount_table(|mt| {
        mt.for_each_child_mount(path, &mut |child_name| {
            if count >= max {
                return false;
            }

            // A mount point always lists as a directory, whatever the entry it
            // shadows was.
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
