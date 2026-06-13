use slopos_ostd::KVec;

use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

const RAMFS_MAX_FILE_SIZE: usize = 16 * 1024 * 1024; // 16 MB per file
use crate::MAX_NAME_LEN;
/// Soft ceiling on the number of inodes a single ramfs instance may hold. The
/// inode pool grows on demand (so a real root filesystem is not capped at a
/// handful of files), but this bound keeps a malformed or hostile initramfs
/// from exhausting kernel memory. It comfortably exceeds any real root.
const RAMFS_MAX_INODES: usize = 4096;

const ROOT_INODE: InodeId = 1;

#[derive(Clone, Copy)]
struct DirEntry {
    name: [u8; MAX_NAME_LEN],
    name_len: usize,
    inode: InodeId,
}

impl DirEntry {
    const fn empty() -> Self {
        Self {
            name: [0; MAX_NAME_LEN],
            name_len: 0,
            inode: 0,
        }
    }
}

struct RamInode {
    in_use: bool,
    file_type: FileType,
    data: KVec<u8>,
    dir_entries: KVec<DirEntry>,
    parent: InodeId,
    mode: u16,
    nlink: u32,
}

impl RamInode {
    fn new() -> Self {
        Self {
            in_use: false,
            file_type: FileType::Regular,
            data: KVec::new(),
            dir_entries: KVec::new(),
            parent: 0,
            mode: 0o644,
            nlink: 1,
        }
    }

    fn reset(&mut self) {
        self.in_use = false;
        self.file_type = FileType::Regular;
        self.data.clear();
        self.data.shrink_to_fit();
        self.dir_entries.clear();
        self.dir_entries.shrink_to_fit();
        self.parent = 0;
        self.mode = 0o644;
        self.nlink = 1;
    }

    fn data_len(&self) -> usize {
        self.data.len()
    }

    fn dir_entry_count(&self) -> usize {
        self.dir_entries.len()
    }

    fn add_dir_entry(&mut self, name: &[u8], inode: InodeId) -> VfsResult<()> {
        for entry in self.dir_entries.iter() {
            if entry.name_len == name.len() && entry.name[..entry.name_len] == *name {
                return Err(VfsError::AlreadyExists);
            }
        }

        let mut entry = DirEntry::empty();
        let len = name.len().min(MAX_NAME_LEN);
        entry.name[..len].copy_from_slice(&name[..len]);
        entry.name_len = len;
        entry.inode = inode;
        self.dir_entries
            .push(entry)
            .map_err(|_| VfsError::NoSpace)?;

        Ok(())
    }

    fn remove_dir_entry(&mut self, name: &[u8]) -> VfsResult<InodeId> {
        for i in 0..self.dir_entries.len() {
            let entry = &self.dir_entries[i];
            if entry.name_len == name.len() && entry.name[..entry.name_len] == *name {
                let inode = entry.inode;
                self.dir_entries.swap_remove(i);
                return Ok(inode);
            }
        }
        Err(VfsError::NotFound)
    }

    fn lookup(&self, name: &[u8]) -> VfsResult<InodeId> {
        for entry in self.dir_entries.iter() {
            if entry.name_len == name.len() && entry.name[..entry.name_len] == *name {
                return Ok(entry.inode);
            }
        }
        Err(VfsError::NotFound)
    }
}

struct RamFsInner {
    inodes: KVec<RamInode>,
    initialized: bool,
}

impl RamFsInner {
    fn new() -> Self {
        let mut inner = Self {
            inodes: KVec::new(),
            initialized: false,
        };
        inner.ensure_initialized();
        inner
    }

    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }
        // Index 0 is a reserved sentinel; inode ids start at ROOT_INODE (1).
        while self.inodes.len() <= ROOT_INODE as usize {
            self.inodes.push(RamInode::new()).expect("ramfs: alloc");
        }
        self.initialized = true;

        let root = &mut self.inodes[ROOT_INODE as usize];
        root.in_use = true;
        root.file_type = FileType::Directory;
        root.mode = 0o755;
        root.nlink = 2;
        root.parent = ROOT_INODE;

        root.add_dir_entry(b".", ROOT_INODE).ok();
        root.add_dir_entry(b"..", ROOT_INODE).ok();
    }

    fn alloc_inode(&mut self) -> VfsResult<InodeId> {
        // Reuse a freed slot if one exists (skip the index-0 sentinel and root).
        for id in (ROOT_INODE as usize + 1)..self.inodes.len() {
            if !self.inodes[id].in_use {
                return Ok(id as InodeId);
            }
        }
        // Otherwise grow the pool, bounded by the soft ceiling.
        if self.inodes.len() >= RAMFS_MAX_INODES {
            return Err(VfsError::NoSpace);
        }
        let id = self.inodes.len();
        self.inodes
            .push(RamInode::new())
            .map_err(|_| VfsError::NoSpace)?;
        Ok(id as InodeId)
    }

    fn get_inode(&self, id: InodeId) -> VfsResult<&RamInode> {
        if id as usize >= self.inodes.len() {
            return Err(VfsError::NotFound);
        }
        let inode = &self.inodes[id as usize];
        if !inode.in_use {
            return Err(VfsError::NotFound);
        }
        Ok(inode)
    }

    fn get_inode_mut(&mut self, id: InodeId) -> VfsResult<&mut RamInode> {
        if id as usize >= self.inodes.len() {
            return Err(VfsError::NotFound);
        }
        let inode = &mut self.inodes[id as usize];
        if !inode.in_use {
            return Err(VfsError::NotFound);
        }
        Ok(inode)
    }
}

pub struct RamFs {
    inner: SpinLock<RamFsInner>,
}

impl RamFs {
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(RamFsInner::new(), LOCK_LEVEL_RESOURCE),
        }
    }

    /// Const-constructible version for statics. Inode storage is allocated
    /// lazily on first access via `ensure_initialized`.
    pub const fn new_const() -> Self {
        Self {
            inner: SpinLock::new(
                RamFsInner {
                    inodes: KVec::new(),
                    initialized: false,
                },
                LOCK_LEVEL_RESOURCE,
            ),
        }
    }

    fn with_inner<R>(&self, f: impl FnOnce(&RamFsInner) -> R) -> R {
        let mut inner = self.inner.lock();
        inner.ensure_initialized();
        f(&*inner)
    }

    fn with_inner_mut<R>(&self, f: impl FnOnce(&mut RamFsInner) -> R) -> R {
        let mut inner = self.inner.lock();
        inner.ensure_initialized();
        f(&mut *inner)
    }
}

impl Default for RamFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for RamFs {
    fn name(&self) -> &'static str {
        "ramfs"
    }

    fn root_inode(&self) -> InodeId {
        ROOT_INODE
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        self.with_inner(|inner| {
            let parent_inode = inner.get_inode(parent)?;

            if parent_inode.file_type != FileType::Directory {
                return Err(VfsError::NotDirectory);
            }

            parent_inode.lookup(name)
        })
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        self.with_inner(|inner| {
            let ram_inode = inner.get_inode(inode)?;

            Ok(FileStat {
                inode,
                file_type: ram_inode.file_type,
                size: ram_inode.data_len() as u64,
                mode: ram_inode.mode,
                nlink: ram_inode.nlink,
                uid: 0,
                gid: 0,
                atime: 0,
                mtime: 0,
                ctime: 0,
                dev_major: 0,
                dev_minor: 0,
            })
        })
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.with_inner(|inner| {
            let ram_inode = inner.get_inode(inode)?;

            if ram_inode.file_type == FileType::Directory {
                return Err(VfsError::IsDirectory);
            }

            let offset = offset as usize;
            if offset >= ram_inode.data_len() {
                return Ok(0);
            }

            let available = ram_inode.data_len() - offset;
            let to_read = buf.len().min(available);
            buf[..to_read].copy_from_slice(&ram_inode.data[offset..offset + to_read]);
            Ok(to_read)
        })
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.with_inner_mut(|inner| {
            let ram_inode = inner.get_inode_mut(inode)?;

            if ram_inode.file_type == FileType::Directory {
                return Err(VfsError::IsDirectory);
            }

            let offset = offset as usize;
            let end = offset + buf.len();

            if end > RAMFS_MAX_FILE_SIZE {
                return Err(VfsError::NoSpace);
            }

            // Grow the data vector if needed
            if end > ram_inode.data.len() {
                ram_inode
                    .data
                    .resize(end, 0)
                    .map_err(|_| VfsError::NoSpace)?;
            }

            ram_inode.data[offset..end].copy_from_slice(buf);

            Ok(buf.len())
        })
    }

    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        self.with_inner_mut(|inner| {
            {
                let parent_inode = inner.get_inode(parent)?;
                if parent_inode.file_type != FileType::Directory {
                    return Err(VfsError::NotDirectory);
                }
                if parent_inode.lookup(name).is_ok() {
                    return Err(VfsError::AlreadyExists);
                }
            }

            let new_id = inner.alloc_inode()?;

            {
                let new_inode = &mut inner.inodes[new_id as usize];
                new_inode.in_use = true;
                new_inode.file_type = file_type;
                new_inode.data.clear();
                new_inode.dir_entries.clear();
                new_inode.parent = parent;

                match file_type {
                    FileType::Directory => {
                        new_inode.mode = 0o755;
                        new_inode.nlink = 2;
                        new_inode.add_dir_entry(b".", new_id)?;
                        new_inode.add_dir_entry(b"..", parent)?;
                    }
                    _ => {
                        new_inode.mode = 0o644;
                        new_inode.nlink = 1;
                    }
                }
            }

            inner.get_inode_mut(parent)?.add_dir_entry(name, new_id)?;

            if file_type == FileType::Directory {
                inner.get_inode_mut(parent)?.nlink += 1;
            }

            Ok(new_id)
        })
    }

    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        self.with_inner_mut(|inner| {
            let target_id = {
                let parent_inode = inner.get_inode(parent)?;
                if parent_inode.file_type != FileType::Directory {
                    return Err(VfsError::NotDirectory);
                }
                parent_inode.lookup(name)?
            };

            let is_dir = {
                let target = inner.get_inode(target_id)?;
                if target.file_type == FileType::Directory && target.dir_entry_count() > 2 {
                    return Err(VfsError::NotEmpty);
                }
                target.file_type == FileType::Directory
            };

            inner.get_inode_mut(parent)?.remove_dir_entry(name)?;

            if is_dir {
                inner.get_inode_mut(parent)?.nlink -= 1;
            }

            inner.inodes[target_id as usize].reset();

            Ok(())
        })
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        self.with_inner(|inner| {
            let ram_inode = inner.get_inode(inode)?;

            if ram_inode.file_type != FileType::Directory {
                return Err(VfsError::NotDirectory);
            }

            let mut count = 0;
            for i in offset..ram_inode.dir_entries.len() {
                let entry = &ram_inode.dir_entries[i];
                let entry_inode = match inner.get_inode(entry.inode) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let name = &entry.name[..entry.name_len];
                if !callback(name, entry.inode, entry_inode.file_type) {
                    break;
                }
                count += 1;
            }

            Ok(count)
        })
    }

    fn truncate(&self, inode: InodeId, size: u64) -> VfsResult<()> {
        self.with_inner_mut(|inner| {
            let ram_inode = inner.get_inode_mut(inode)?;

            if ram_inode.file_type == FileType::Directory {
                return Err(VfsError::IsDirectory);
            }

            let new_size = (size as usize).min(RAMFS_MAX_FILE_SIZE);
            ram_inode
                .data
                .resize(new_size, 0)
                .map_err(|_| VfsError::NoSpace)?;

            Ok(())
        })
    }

    fn rename(
        &self,
        old_parent: InodeId,
        old_name: &[u8],
        new_parent: InodeId,
        new_name: &[u8],
    ) -> VfsResult<()> {
        self.with_inner_mut(|inner| {
            if old_parent == new_parent && old_name == new_name {
                return Ok(());
            }

            let target_inode = {
                let old_parent_node = inner.get_inode(old_parent)?;
                if old_parent_node.file_type != FileType::Directory {
                    return Err(VfsError::NotDirectory);
                }
                old_parent_node.lookup(old_name)?
            };

            {
                let new_parent_node = inner.get_inode(new_parent)?;
                if new_parent_node.file_type != FileType::Directory {
                    return Err(VfsError::NotDirectory);
                }
                if new_parent_node.lookup(new_name).is_ok() {
                    inner
                        .get_inode_mut(new_parent)?
                        .remove_dir_entry(new_name)?;
                }
            }

            inner
                .get_inode_mut(old_parent)?
                .remove_dir_entry(old_name)?;
            inner
                .get_inode_mut(new_parent)?
                .add_dir_entry(new_name, target_inode)?;

            let is_dir = inner.get_inode(target_inode)?.file_type == FileType::Directory;
            if is_dir {
                let target_node = inner.get_inode_mut(target_inode)?;
                for i in 0..target_node.dir_entries.len() {
                    if target_node.dir_entries[i].name_len == 2
                        && target_node.dir_entries[i].name[0] == b'.'
                        && target_node.dir_entries[i].name[1] == b'.'
                    {
                        target_node.dir_entries[i].inode = new_parent;
                        break;
                    }
                }
                target_node.parent = new_parent;

                if old_parent != new_parent {
                    let old_nlink = inner.get_inode(old_parent)?.nlink;
                    inner.get_inode_mut(old_parent)?.nlink = old_nlink.saturating_sub(1);

                    let new_nlink = inner.get_inode(new_parent)?.nlink;
                    inner.get_inode_mut(new_parent)?.nlink = new_nlink.saturating_add(1);
                }
            }

            Ok(())
        })
    }

    fn set_mode(&self, inode: InodeId, mode: u16) -> VfsResult<()> {
        self.with_inner_mut(|inner| {
            let ram_inode = inner.get_inode_mut(inode)?;
            ram_inode.mode = mode & 0o7777;
            Ok(())
        })
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
}
