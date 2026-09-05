use slopos_ostd::KVec;

use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
use slopos_ostd::sync::SpinLock;
use slopos_ostd::sync::lock_tracking::LockClassKey;

const RAMFS_MAX_FILE_SIZE: usize = 16 * 1024 * 1024;
use crate::MAX_NAME_LEN;
/// Soft ceiling on the number of inodes a single ramfs instance may hold. The
/// pool grows on demand, but the bound keeps a malformed or hostile initramfs
/// from exhausting kernel memory.
const RAMFS_MAX_INODES: usize = 4096;

const ROOT_SLOT: usize = 1;

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

/// Slot index and generation packed into the [`InodeId`] a descriptor holds.
///
/// Slots are recycled the moment an inode is unlinked, so an id that named
/// only a slot would silently follow the *next* file created in it. The
/// generation moves on every recycle, so a descriptor outliving its file
/// fails instead of aliasing a stranger's.
const INODE_GEN_SHIFT: u32 = 32;
const INODE_SLOT_MASK: u64 = 0xFFFF_FFFF;

#[inline]
fn pack_inode_id(slot: u64, generation: u32) -> InodeId {
    ((generation as u64) << INODE_GEN_SHIFT) | (slot & INODE_SLOT_MASK)
}

#[inline]
fn inode_slot(id: InodeId) -> usize {
    (id & INODE_SLOT_MASK) as usize
}

#[inline]
fn inode_generation(id: InodeId) -> u32 {
    (id >> INODE_GEN_SHIFT) as u32
}

struct RamInode {
    in_use: bool,
    file_type: FileType,
    data: KVec<u8>,
    dir_entries: KVec<DirEntry>,
    parent: InodeId,
    mode: u16,
    nlink: u32,
    /// Refuses every mutation once set; never cleared while the inode lives.
    sealed: bool,
    /// Bumped on every reset, so a stale id fails to resolve.
    generation: u32,
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
            sealed: false,
            generation: 1,
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
        self.sealed = false;
        self.generation = self.generation.wrapping_add(1);
    }

    fn data_len(&self) -> usize {
        self.data.len()
    }

    fn dir_entry_count(&self) -> usize {
        self.dir_entries.len()
    }

    fn add_dir_entry(&mut self, name: &[u8], inode: InodeId) -> VfsResult<()> {
        // Truncating instead would store a name no lookup can match, since
        // every comparison here tests `name_len == name.len()`.
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        for entry in self.dir_entries.iter() {
            if entry.name_len == name.len() && entry.name[..entry.name_len] == *name {
                return Err(VfsError::AlreadyExists);
            }
        }

        let mut entry = DirEntry::empty();
        let len = name.len();
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
    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }
        // Index 0 is a reserved sentinel; inode slots start at ROOT_SLOT (1).
        while self.inodes.len() <= ROOT_SLOT {
            self.inodes.push(RamInode::new()).expect("ramfs: alloc");
        }
        self.initialized = true;

        let root_id = pack_inode_id(ROOT_SLOT as u64, self.inodes[ROOT_SLOT].generation);
        let root = &mut self.inodes[ROOT_SLOT];
        root.in_use = true;
        root.file_type = FileType::Directory;
        root.mode = 0o755;
        root.nlink = 2;
        root.parent = root_id;

        root.add_dir_entry(b".", root_id).ok();
        root.add_dir_entry(b"..", root_id).ok();
    }

    fn alloc_inode(&mut self) -> VfsResult<InodeId> {
        for slot in (ROOT_SLOT + 1)..self.inodes.len() {
            if !self.inodes[slot].in_use {
                return Ok(pack_inode_id(slot as u64, self.inodes[slot].generation));
            }
        }
        if self.inodes.len() >= RAMFS_MAX_INODES {
            return Err(VfsError::NoSpace);
        }
        let slot = self.inodes.len();
        self.inodes
            .push(RamInode::new())
            .map_err(|_| VfsError::NoSpace)?;
        Ok(pack_inode_id(slot as u64, self.inodes[slot].generation))
    }

    fn get_inode(&self, id: InodeId) -> VfsResult<&RamInode> {
        let slot = inode_slot(id);
        if slot >= self.inodes.len() {
            return Err(VfsError::NotFound);
        }
        let inode = &self.inodes[slot];
        if !inode.in_use || inode.generation != inode_generation(id) {
            return Err(VfsError::NotFound);
        }
        Ok(inode)
    }

    fn get_inode_mut(&mut self, id: InodeId) -> VfsResult<&mut RamInode> {
        let slot = inode_slot(id);
        if slot >= self.inodes.len() {
            return Err(VfsError::NotFound);
        }
        let inode = &mut self.inodes[slot];
        if !inode.in_use || inode.generation != inode_generation(id) {
            return Err(VfsError::NotFound);
        }
        Ok(inode)
    }

    /// Is `maybe_ancestor` at or above `start` in the directory tree?
    ///
    /// Walks parent links, bounded by the table size so a pre-existing cycle
    /// terminates the walk instead of hanging it.
    fn is_ancestor_of(&self, maybe_ancestor: InodeId, start: InodeId) -> VfsResult<bool> {
        let root = self.root_id();
        let mut current = start;
        for _ in 0..RAMFS_MAX_INODES {
            if current == maybe_ancestor {
                return Ok(true);
            }
            if current == root {
                return Ok(false);
            }
            current = self.get_inode(current)?.parent;
        }
        Ok(true)
    }

    /// The root's id carries its generation like any other.
    fn root_id(&self) -> InodeId {
        let generation = self
            .inodes
            .get(ROOT_SLOT)
            .map(|i| i.generation)
            .unwrap_or(1);
        pack_inode_id(ROOT_SLOT as u64, generation)
    }
}

pub struct RamFs {
    inner: SpinLock<RamFsInner>,
}

impl RamFs {
    /// Inode storage is allocated lazily on first access.
    ///
    /// The lock class comes from the caller: a path walk that crosses a mount
    /// point holds one mount's lock while taking another's, and two mounts
    /// sharing a class would make that legal-but-unordered nesting.
    pub const fn new_const(class: &'static LockClassKey) -> Self {
        Self {
            inner: SpinLock::new(
                RamFsInner {
                    inodes: KVec::new(),
                    initialized: false,
                },
                class,
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

impl FileSystem for RamFs {
    fn name(&self) -> &'static str {
        "ramfs"
    }

    fn root_inode(&self) -> InodeId {
        self.with_inner(|inner| inner.root_id())
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
                sealed: ram_inode.sealed,
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

            if ram_inode.sealed {
                return Err(VfsError::PermissionDenied);
            }
            if ram_inode.file_type == FileType::Directory {
                return Err(VfsError::IsDirectory);
            }

            let offset = offset as usize;
            let end = offset + buf.len();

            if end > RAMFS_MAX_FILE_SIZE {
                return Err(VfsError::NoSpace);
            }

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
                let new_inode = &mut inner.inodes[inode_slot(new_id)];
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

            inner.inodes[inode_slot(target_id)].reset();

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

    /// Overridden because [`Self::readdir`] skips an entry whose inode fails
    /// to resolve without invoking the callback: the trait's default counts
    /// callbacks, so after one such skip its cookie would lag the index it
    /// feeds back and the next page would repeat a name. Here the cookie *is*
    /// the index, so a skip costs nothing.
    fn readdir_cookie(
        &self,
        inode: InodeId,
        cookie: u64,
        callback: &mut dyn FnMut(u64, &[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<u64> {
        let start = usize::try_from(cookie).map_err(|_| VfsError::InvalidArgument)?;
        self.with_inner(|inner| {
            let ram_inode = inner.get_inode(inode)?;
            if ram_inode.file_type != FileType::Directory {
                return Err(VfsError::NotDirectory);
            }
            let mut index = start;
            while index < ram_inode.dir_entries.len() {
                let entry = &ram_inode.dir_entries[index];
                index += 1;
                let Ok(entry_inode) = inner.get_inode(entry.inode) else {
                    continue;
                };
                let name = &entry.name[..entry.name_len];
                if !callback(index as u64, name, entry.inode, entry_inode.file_type) {
                    break;
                }
            }
            Ok(index as u64)
        })
    }

    fn truncate(&self, inode: InodeId, size: u64) -> VfsResult<()> {
        self.with_inner_mut(|inner| {
            let ram_inode = inner.get_inode_mut(inode)?;

            if ram_inode.sealed {
                return Err(VfsError::PermissionDenied);
            }
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

            let source_type = inner.get_inode(target_inode)?.file_type;

            // Splicing a directory into its own descendant detaches the whole
            // subtree: it becomes unreachable from the root and unremovable.
            if source_type == FileType::Directory
                && inner.is_ancestor_of(target_inode, new_parent)?
            {
                return Err(VfsError::InvalidPath);
            }

            let displaced = {
                let new_parent_node = inner.get_inode(new_parent)?;
                if new_parent_node.file_type != FileType::Directory {
                    return Err(VfsError::NotDirectory);
                }
                match new_parent_node.lookup(new_name) {
                    Ok(existing) => Some(existing),
                    Err(_) => None,
                }
            };

            if let Some(existing) = displaced {
                let existing_type = inner.get_inode(existing)?.file_type;
                match (source_type, existing_type) {
                    (FileType::Directory, FileType::Directory) => {
                        if inner.get_inode(existing)?.dir_entry_count() > 2 {
                            return Err(VfsError::NotEmpty);
                        }
                    }
                    (FileType::Directory, _) => return Err(VfsError::NotDirectory),
                    (_, FileType::Directory) => return Err(VfsError::IsDirectory),
                    _ => {}
                }
                inner
                    .get_inode_mut(new_parent)?
                    .remove_dir_entry(new_name)?;
                // Unlinking the entry alone leaks the inode out of the fixed
                // table, which repeated overwrites then exhaust.
                inner.inodes[inode_slot(existing)].reset();
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
            if ram_inode.sealed {
                return Err(VfsError::PermissionDenied);
            }
            ram_inode.mode = mode & 0o7777;
            Ok(())
        })
    }

    fn set_sealed(&self, inode: InodeId) -> VfsResult<()> {
        self.with_inner_mut(|inner| {
            inner.get_inode_mut(inode)?.sealed = true;
            Ok(())
        })
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
}
