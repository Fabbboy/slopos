use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
use slopos_lib::IrqMutex;

const MAX_INODES: usize = 64;
const MAX_DATA_SIZE: usize = 4096;
const MAX_NAME_LEN: usize = 32;
const MAX_DIR_ENTRIES: usize = 32;

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
    data: [u8; MAX_DATA_SIZE],
    data_len: usize,
    dir_entries: [DirEntry; MAX_DIR_ENTRIES],
    dir_entry_count: usize,
    parent: InodeId,
    mode: u16,
    nlink: u32,
}

impl RamInode {
    const fn empty() -> Self {
        Self {
            in_use: false,
            file_type: FileType::Regular,
            data: [0; MAX_DATA_SIZE],
            data_len: 0,
            dir_entries: [const { DirEntry::empty() }; MAX_DIR_ENTRIES],
            dir_entry_count: 0,
            parent: 0,
            mode: 0o644,
            nlink: 1,
        }
    }

    fn add_dir_entry(&mut self, name: &[u8], inode: InodeId) -> VfsResult<()> {
        if self.dir_entry_count >= MAX_DIR_ENTRIES {
            return Err(VfsError::NoSpace);
        }

        for i in 0..self.dir_entry_count {
            let entry = &self.dir_entries[i];
            if entry.name_len == name.len() && &entry.name[..name.len()] == name {
                return Err(VfsError::AlreadyExists);
            }
        }

        let entry = &mut self.dir_entries[self.dir_entry_count];
        let len = name.len().min(MAX_NAME_LEN);
        entry.name[..len].copy_from_slice(&name[..len]);
        entry.name_len = len;
        entry.inode = inode;
        self.dir_entry_count += 1;

        Ok(())
    }

    fn remove_dir_entry(&mut self, name: &[u8]) -> VfsResult<InodeId> {
        for i in 0..self.dir_entry_count {
            let entry = &self.dir_entries[i];
            if entry.name_len == name.len() && &entry.name[..name.len()] == name {
                let inode = entry.inode;
                if i < self.dir_entry_count - 1 {
                    self.dir_entries[i] = self.dir_entries[self.dir_entry_count - 1];
                }
                self.dir_entries[self.dir_entry_count - 1] = DirEntry::empty();
                self.dir_entry_count -= 1;
                return Ok(inode);
            }
        }
        Err(VfsError::NotFound)
    }

    fn lookup(&self, name: &[u8]) -> VfsResult<InodeId> {
        for i in 0..self.dir_entry_count {
            let entry = &self.dir_entries[i];
            if entry.name_len == name.len() && &entry.name[..name.len()] == name {
                return Ok(entry.inode);
            }
        }
        Err(VfsError::NotFound)
    }
}

struct RamFsInner {
    inodes: [RamInode; MAX_INODES],
    next_inode: InodeId,
}

impl RamFsInner {
    const fn new_const() -> Self {
        Self {
            inodes: [const { RamInode::empty() }; MAX_INODES],
            next_inode: ROOT_INODE + 1,
        }
    }

    fn new() -> Self {
        let mut inner = Self {
            inodes: [const { RamInode::empty() }; MAX_INODES],
            next_inode: ROOT_INODE + 1,
        };

        let root = &mut inner.inodes[ROOT_INODE as usize];
        root.in_use = true;
        root.file_type = FileType::Directory;
        root.mode = 0o755;
        root.nlink = 2;
        root.parent = ROOT_INODE;

        root.add_dir_entry(b".", ROOT_INODE).ok();
        root.add_dir_entry(b"..", ROOT_INODE).ok();

        inner
    }

    fn alloc_inode(&mut self) -> VfsResult<InodeId> {
        for _ in 0..MAX_INODES {
            let id = self.next_inode;
            self.next_inode = if self.next_inode as usize >= MAX_INODES - 1 {
                ROOT_INODE + 1
            } else {
                self.next_inode + 1
            };

            if (id as usize) < MAX_INODES && !self.inodes[id as usize].in_use {
                return Ok(id);
            }
        }
        Err(VfsError::NoSpace)
    }

    fn get_inode(&self, id: InodeId) -> VfsResult<&RamInode> {
        if id as usize >= MAX_INODES {
            return Err(VfsError::NotFound);
        }
        let inode = &self.inodes[id as usize];
        if !inode.in_use {
            return Err(VfsError::NotFound);
        }
        Ok(inode)
    }

    fn get_inode_mut(&mut self, id: InodeId) -> VfsResult<&mut RamInode> {
        if id as usize >= MAX_INODES {
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
    inner: IrqMutex<RamFsInner>,
}

impl RamFs {
    pub fn new() -> Self {
        Self {
            inner: IrqMutex::new(RamFsInner::new()),
        }
    }

    pub const fn new_const() -> Self {
        Self {
            inner: IrqMutex::new(RamFsInner::new_const()),
        }
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
        let inner = self.inner.lock();
        let parent_inode = inner.get_inode(parent)?;

        if parent_inode.file_type != FileType::Directory {
            return Err(VfsError::NotDirectory);
        }

        parent_inode.lookup(name)
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        let inner = self.inner.lock();
        let ram_inode = inner.get_inode(inode)?;

        Ok(FileStat {
            inode,
            file_type: ram_inode.file_type,
            size: ram_inode.data_len as u64,
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
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let inner = self.inner.lock();
        let ram_inode = inner.get_inode(inode)?;

        if ram_inode.file_type == FileType::Directory {
            return Err(VfsError::IsDirectory);
        }

        let offset = offset as usize;
        if offset >= ram_inode.data_len {
            return Ok(0);
        }

        let available = ram_inode.data_len - offset;
        let to_read = buf.len().min(available);
        buf[..to_read].copy_from_slice(&ram_inode.data[offset..offset + to_read]);

        Ok(to_read)
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        let mut inner = self.inner.lock();
        let ram_inode = inner.get_inode_mut(inode)?;

        if ram_inode.file_type == FileType::Directory {
            return Err(VfsError::IsDirectory);
        }

        let offset = offset as usize;
        let end = offset + buf.len();

        if end > MAX_DATA_SIZE {
            return Err(VfsError::NoSpace);
        }

        ram_inode.data[offset..end].copy_from_slice(buf);
        if end > ram_inode.data_len {
            ram_inode.data_len = end;
        }

        Ok(buf.len())
    }

    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        let mut inner = self.inner.lock();

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
            new_inode.data_len = 0;
            new_inode.dir_entry_count = 0;
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
    }

    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        let mut inner = self.inner.lock();

        let target_id = {
            let parent_inode = inner.get_inode(parent)?;
            if parent_inode.file_type != FileType::Directory {
                return Err(VfsError::NotDirectory);
            }
            parent_inode.lookup(name)?
        };

        let is_dir = {
            let target = inner.get_inode(target_id)?;
            if target.file_type == FileType::Directory && target.dir_entry_count > 2 {
                return Err(VfsError::NotEmpty);
            }
            target.file_type == FileType::Directory
        };

        inner.get_inode_mut(parent)?.remove_dir_entry(name)?;

        if is_dir {
            inner.get_inode_mut(parent)?.nlink -= 1;
        }

        inner.inodes[target_id as usize] = RamInode::empty();

        Ok(())
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        let inner = self.inner.lock();
        let ram_inode = inner.get_inode(inode)?;

        if ram_inode.file_type != FileType::Directory {
            return Err(VfsError::NotDirectory);
        }

        let mut count = 0;
        for i in offset..ram_inode.dir_entry_count {
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
    }

    fn truncate(&self, inode: InodeId, size: u64) -> VfsResult<()> {
        let mut inner = self.inner.lock();
        let ram_inode = inner.get_inode_mut(inode)?;

        if ram_inode.file_type == FileType::Directory {
            return Err(VfsError::IsDirectory);
        }

        let new_size = (size as usize).min(MAX_DATA_SIZE);
        if new_size < ram_inode.data_len {
            ram_inode.data[new_size..ram_inode.data_len].fill(0);
        }
        ram_inode.data_len = new_size;

        Ok(())
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
}

unsafe impl Send for RamFs {}
unsafe impl Sync for RamFs {}
