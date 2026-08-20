pub mod blockmap;
pub mod cache;
pub mod dir;
#[path = "alloc.rs"]
pub mod ext2_alloc;
pub mod file;
pub mod inode;
pub mod ondisk;
pub mod symlink;
pub mod time;
pub mod types;

use cache::BlockCache;
use ondisk::{
    DIR_FT_DIR, DIR_FT_REG_FILE, DirEntry, EXT2_ERROR_FS, EXT2_VALID_FS, GroupDesc, Inode,
    MODE_DIRECTORY, MODE_FILE, Superblock,
};
use types::{BlockNum, GroupIdx, InodeNum};

use crate::blockdev::BlockDevice;

pub use ondisk::EXT2_MAX_BLOCK_SIZE;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Ext2Error {
    InvalidSuperblock,
    UnsupportedBlockSize,
    /// The image declares an incompatible feature this implementation cannot
    /// represent; mounting it read-write would corrupt it.
    UnsupportedFeature,
    /// The mount is read-only, either by request or because the image carries
    /// an unsupported read-only-compatible feature.
    ReadOnly,
    InvalidInode,
    InvalidBlock,
    UnsupportedIndirection,
    DeviceError,
    DirectoryFormat,
    NotDirectory,
    NotFile,
    PathNotFound,
    NoSpace,
    NameTooLong,
    AlreadyExists,
    NotEmpty,
    IsDirectory,
    TooManyLinks,
    OutOfMemory,
}

pub type Ext2Superblock = Superblock;
pub type Ext2Inode = Inode;

pub struct Ext2Fs<'a> {
    device: &'a dyn BlockDevice,
    superblock: Superblock,
    cache: &'a mut BlockCache,
    block_size: u32,
    inode_size: u16,
    ptrs_per_block: u32,
    /// Set when an op changed the in-memory free-counts, leaving the on-disk
    /// superblock stale; persisted only by [`Self::sync`], never mid-operation.
    superblock_dirty: bool,
    /// Refuses every mutating entry point. Set when the image declares a
    /// read-only-compatible feature this implementation does not write.
    read_only: bool,
}

impl<'a> Ext2Fs<'a> {
    /// Reads the superblock *without* a cache: mount needs `block_size` to size
    /// the [`BlockCache`] before it exists. Returns
    /// `(superblock, block_size, inode_size)`.
    pub fn mount_params(device: &dyn BlockDevice) -> Result<(Superblock, u32, u16), Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        let superblock = Superblock::parse(&sb_buf)?;
        let block_size = superblock.block_size()?;
        let inode_size = superblock.effective_inode_size();
        Ok((superblock, block_size, inode_size))
    }

    /// **Performs no allocation** — the cache lives in the long-lived FS state
    /// and is merely borrowed, so building one of these per VFS call is free.
    pub fn new(
        device: &'a dyn BlockDevice,
        cache: &'a mut BlockCache,
        superblock: Superblock,
        block_size: u32,
        inode_size: u16,
    ) -> Self {
        let read_only = superblock.requires_readonly();
        Self {
            device,
            superblock,
            cache,
            block_size,
            inode_size,
            ptrs_per_block: block_size / 4,
            superblock_dirty: false,
            read_only,
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Gate for every mutating entry point.
    fn check_writable(&self) -> Result<(), Ext2Error> {
        if self.read_only {
            return Err(Ext2Error::ReadOnly);
        }
        Ok(())
    }

    /// Mark the image as not cleanly unmounted, so a later fsck knows it must
    /// run. Cleared again by [`Self::mark_clean`] on a clean unmount.
    pub fn mark_dirty_on_disk(&mut self) -> Result<(), Ext2Error> {
        if self.read_only || self.superblock.state == EXT2_ERROR_FS {
            return Ok(());
        }
        self.superblock.state = EXT2_ERROR_FS;
        self.write_superblock_state()
    }

    pub fn mark_clean(&mut self) -> Result<(), Ext2Error> {
        if self.read_only {
            return Ok(());
        }
        self.superblock.state = EXT2_VALID_FS;
        self.write_superblock_state()
    }

    fn write_superblock_state(&mut self) -> Result<(), Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        self.device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        sb_buf[58..60].copy_from_slice(&self.superblock.state.to_le_bytes());
        self.device
            .write_at(1024, &sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        Ok(())
    }

    pub fn superblock(&self) -> Superblock {
        self.superblock
    }

    pub fn superblock_dirty(&self) -> bool {
        self.superblock_dirty
    }

    /// The flag must survive across handles: a mutating op dirties it in one
    /// `with_fs` call and the flusher persists it from a *different* handle.
    pub fn set_superblock_dirty(&mut self, dirty: bool) {
        self.superblock_dirty = dirty;
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn dirty_count(&self) -> usize {
        self.cache.dirty_count()
    }

    /// Ordered durability, following the ext2 `data=ordered` discipline (minus a
    /// metadata journal): data blocks, barrier, metadata blocks, barrier,
    /// superblock free-counts, barrier. A crash between phases can leave
    /// recoverable free-count drift but never a directory entry or inode
    /// pointing at uninitialised on-disk data.
    pub fn sync(&mut self) -> Result<(), Ext2Error> {
        self.cache.flush_kind(cache::BlockKind::Data, self.device)?;
        self.device_barrier()?;
        self.cache
            .flush_kind(cache::BlockKind::Metadata, self.device)?;
        self.device_barrier()?;
        if self.superblock_dirty {
            self.write_superblock()?;
            self.device_barrier()?;
            self.superblock_dirty = false;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), Ext2Error> {
        self.sync()
    }

    fn device_barrier(&self) -> Result<(), Ext2Error> {
        self.device.flush().map_err(|_| Ext2Error::DeviceError)
    }

    pub fn read_inode(&mut self, ino: u32) -> Result<Inode, Ext2Error> {
        self.read_inode_num(InodeNum(ino))
    }

    fn read_inode_num(&mut self, ino: InodeNum) -> Result<Inode, Ext2Error> {
        if !ino.is_valid() || ino.raw() > self.superblock.inodes_count {
            return Err(Ext2Error::InvalidInode);
        }
        let group = ino.block_group(self.superblock.inodes_per_group);
        let local = ino.local_index(self.superblock.inodes_per_group);
        let desc = self.read_group_desc(group)?;
        if !desc.inode_table.is_valid() {
            return Err(Ext2Error::InvalidInode);
        }
        let byte_off = desc.inode_table.to_disk_offset(self.block_size).raw()
            + (local as u64 * self.inode_size as u64);
        let blk_num = BlockNum((byte_off / self.block_size as u64) as u32);
        let within = (byte_off % self.block_size as u64) as usize;
        let block = self.cache.get(blk_num, self.device)?;
        Ok(Inode::parse(
            &block.data()[within..within + self.inode_size as usize],
        ))
    }

    fn write_inode_num(&mut self, ino: InodeNum, inode: &Inode) -> Result<(), Ext2Error> {
        if !ino.is_valid() || ino.raw() > self.superblock.inodes_count {
            return Err(Ext2Error::InvalidInode);
        }
        let group = ino.block_group(self.superblock.inodes_per_group);
        let local = ino.local_index(self.superblock.inodes_per_group);
        let desc = self.read_group_desc(group)?;
        let byte_off = desc.inode_table.to_disk_offset(self.block_size).raw()
            + (local as u64 * self.inode_size as u64);
        let blk_num = BlockNum((byte_off / self.block_size as u64) as u32);
        let within = (byte_off % self.block_size as u64) as usize;
        let mut block = self.cache.get(blk_num, self.device)?;
        inode.encode(&mut block.data_mut()[within..within + self.inode_size as usize]);
        Ok(())
    }

    pub fn read_file(
        &mut self,
        ino: u32,
        offset: u32,
        buffer: &mut [u8],
    ) -> Result<usize, Ext2Error> {
        let inode = self.read_inode_num(InodeNum(ino))?;
        file::read_file(
            &inode,
            offset as u64,
            buffer,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
        )
    }

    pub fn write_file(&mut self, ino: u32, offset: u32, buffer: &[u8]) -> Result<usize, Ext2Error> {
        self.check_writable()?;
        let ino_num = InodeNum(ino);
        let mut inode = self.read_inode_num(ino_num)?;
        let free_before = self.superblock.free_blocks_count;
        let result = file::write_file(
            &mut inode,
            offset as u64,
            buffer,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            &mut self.superblock,
        );
        self.write_inode_num(ino_num, &inode)?;
        if self.superblock.free_blocks_count != free_before {
            self.superblock_dirty = true;
        }
        result
    }

    pub fn for_each_dir_entry<F>(&mut self, ino: u32, mut f: F) -> Result<(), Ext2Error>
    where
        F: FnMut(DirEntry<'_>) -> bool,
    {
        let inode = self.read_inode_num(InodeNum(ino))?;
        dir::for_each_entry(
            &inode,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            &mut f,
        )
    }

    pub fn resolve_path(&mut self, path: &[u8]) -> Result<u32, Ext2Error> {
        if path.is_empty() || path[0] != b'/' {
            return Err(Ext2Error::PathNotFound);
        }
        let mut current = InodeNum::ROOT;
        for component in path[1..].split(|&b| b == b'/') {
            if component.is_empty() || component == b"." {
                continue;
            }
            let inode = self.read_inode_num(current)?;
            if !inode.is_directory() {
                return Err(Ext2Error::NotDirectory);
            }
            if component == b".." {
                let mut parent = None;
                dir::for_each_entry(
                    &inode,
                    &mut *self.cache,
                    self.device,
                    self.ptrs_per_block,
                    self.block_size,
                    &mut |e| {
                        if e.name == b".." {
                            parent = Some(e.inode);
                            false
                        } else {
                            true
                        }
                    },
                )?;
                current = parent.unwrap_or(current);
            } else {
                current = dir::lookup_child(
                    &inode,
                    component,
                    &mut *self.cache,
                    self.device,
                    self.ptrs_per_block,
                    self.block_size,
                )?;
            }
        }
        Ok(current.raw())
    }

    pub fn create_file(&mut self, parent: u32, name: &[u8]) -> Result<u32, Ext2Error> {
        self.create_inode_entry(InodeNum(parent), name, false)
            .map(|n| n.raw())
    }

    pub fn create_directory(&mut self, parent: u32, name: &[u8]) -> Result<u32, Ext2Error> {
        self.create_inode_entry(InodeNum(parent), name, true)
            .map(|n| n.raw())
    }

    fn create_inode_entry(
        &mut self,
        parent_num: InodeNum,
        name: &[u8],
        is_dir: bool,
    ) -> Result<InodeNum, Ext2Error> {
        self.check_writable()?;
        if name.is_empty() || name.len() > 255 {
            return Err(Ext2Error::NameTooLong);
        }
        let mut parent = self.read_inode_num(parent_num)?;
        if !parent.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }

        // Without this a second create writes a second record under the same
        // name: lookup answers whichever comes first and the other inode is
        // unreachable, leaving the image inconsistent for every other ext2
        // implementation.
        if dir::lookup_child(
            &parent,
            name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
        )
        .is_ok()
        {
            return Err(Ext2Error::AlreadyExists);
        }

        let new_ino = ext2_alloc::allocate_inode(
            parent_num.block_group(self.superblock.inodes_per_group),
            &mut self.superblock,
            &mut *self.cache,
            self.device,
            self.block_size,
        )?;

        let mut new_inode = Inode {
            mode: if is_dir {
                MODE_DIRECTORY | 0o755
            } else {
                MODE_FILE | 0o644
            },
            uid: 0,
            size: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            links_count: if is_dir { 2 } else { 1 },
            blocks: 0,
            flags: 0,
            block: [BlockNum::ZERO; 15],
        };

        if is_dir {
            let first_block = ext2_alloc::allocate_block(
                &mut self.superblock,
                &mut *self.cache,
                self.device,
                self.block_size,
            )?;
            {
                let mut blk = self.cache.get_zero(first_block, self.device)?;
                let data = blk.data_mut();
                let bs = self.block_size as usize;
                let dot_rec = 12;
                ondisk::write_dir_entry(&mut data[..dot_rec], new_ino, b".", DIR_FT_DIR, dot_rec);
                let dotdot_rec = bs - dot_rec;
                ondisk::write_dir_entry(
                    &mut data[dot_rec..dot_rec + dotdot_rec],
                    parent_num,
                    b"..",
                    DIR_FT_DIR,
                    dotdot_rec,
                );
            }
            new_inode.block[0] = first_block;
            new_inode.size = self.block_size;
            new_inode.blocks = self.block_size / 512;
        }

        self.write_inode_num(new_ino, &new_inode)?;

        let ft = if is_dir { DIR_FT_DIR } else { DIR_FT_REG_FILE };
        dir::append_dir_entry(
            &mut parent,
            new_ino,
            name,
            ft,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            &mut self.superblock,
        )?;

        if is_dir {
            parent.links_count += 1;
        }
        self.write_inode_num(parent_num, &parent)?;
        self.superblock_dirty = true;

        Ok(new_ino)
    }

    pub fn unlink_entry(&mut self, parent: u32, name: &[u8]) -> Result<(), Ext2Error> {
        self.check_writable()?;
        let parent_num = InodeNum(parent);
        let mut parent_inode = self.read_inode_num(parent_num)?;
        if !parent_inode.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }

        let target_num = dir::lookup_child(
            &parent_inode,
            name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
        )?;
        let target = self.read_inode_num(target_num)?;

        if target.is_directory() {
            return Err(Ext2Error::IsDirectory);
        }

        dir::remove_dir_entry(
            &parent_inode,
            name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
        )?;

        self.release_file_blocks(&target)?;
        ext2_alloc::free_inode(
            target_num,
            &mut self.superblock,
            &mut *self.cache,
            self.device,
            self.block_size,
        )?;

        let zeroed = Inode {
            mode: 0,
            uid: 0,
            size: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            links_count: 0,
            blocks: 0,
            flags: 0,
            block: [BlockNum::ZERO; 15],
        };
        self.write_inode_num(target_num, &zeroed)?;

        // Re-read parent in case dir ops mutated the cached block.
        parent_inode = self.read_inode_num(parent_num)?;
        self.write_inode_num(parent_num, &parent_inode)?;
        self.superblock_dirty = true;

        Ok(())
    }

    pub fn remove_path(&mut self, path: &[u8]) -> Result<(), Ext2Error> {
        let (parent_path, name) = ondisk::split_parent(path).ok_or(Ext2Error::PathNotFound)?;
        let parent = self.resolve_path(parent_path)?;
        self.unlink_entry(parent, name)
    }

    fn read_group_desc(&mut self, group: GroupIdx) -> Result<GroupDesc, Ext2Error> {
        let table_start: u32 = if self.block_size == 1024 { 2 } else { 1 };
        let desc_per_block = self.block_size as usize / 32;
        let block_idx = group.raw() as usize / desc_per_block;
        let within = (group.raw() as usize % desc_per_block) * 32;
        let blk_num = BlockNum(table_start + block_idx as u32);
        let block = self.cache.get(blk_num, self.device)?;
        Ok(GroupDesc::parse(&block.data()[within..within + 32]))
    }

    /// Free every block an inode owns: the twelve direct ones and all three
    /// indirect trees. Missing depths 2 and 3 leaks every block past the
    /// single-indirect reach on each delete, with no way to recover the space
    /// short of reformatting.
    fn release_file_blocks(&mut self, inode: &Inode) -> Result<(), Ext2Error> {
        for blk in inode.block.iter().take(12) {
            if blk.is_valid() {
                ext2_alloc::free_block(
                    *blk,
                    &mut self.superblock,
                    &mut *self.cache,
                    self.device,
                    self.block_size,
                )?;
            }
        }

        let mut freed: slopos_ostd::KVec<BlockNum> = slopos_ostd::KVec::new();
        for (slot, depth) in [(12usize, 1u32), (13, 2), (14, 3)] {
            let root = inode.block[slot];
            if !root.is_valid() {
                continue;
            }
            // `free_indirect` hands back every block it walks; the frees are
            // applied afterwards because the superblock and the cache it
            // needs are borrowed for the walk.
            file::free_indirect(
                root,
                depth,
                &mut self.cache,
                self.device,
                self.ptrs_per_block,
                &mut |b| {
                    freed.push(b).map_err(|_| Ext2Error::OutOfMemory)?;
                    Ok(())
                },
            )?;
            freed.push(root).map_err(|_| Ext2Error::OutOfMemory)?;
        }

        for blk in freed.iter() {
            ext2_alloc::free_block(
                *blk,
                &mut self.superblock,
                &mut *self.cache,
                self.device,
                self.block_size,
            )?;
        }

        Ok(())
    }

    fn write_superblock(&mut self) -> Result<(), Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        self.device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        self.superblock.encode_free_counts(&mut sb_buf);
        self.device
            .write_at(1024, &sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        Ok(())
    }
}
