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
    DIR_FT_DIR, DIR_FT_REG_FILE, DirEntry, GroupDesc, Inode, MODE_DIRECTORY, MODE_FILE, Superblock,
};
use types::{BlockNum, GroupIdx, InodeNum};

use crate::blockdev::BlockDevice;

pub use ondisk::EXT2_MAX_BLOCK_SIZE;

// ---- Error type ----

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Ext2Error {
    InvalidSuperblock,
    UnsupportedBlockSize,
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

// ---- Backward-compat type aliases for ext2_vfs.rs / tests.rs ----

pub type Ext2Superblock = Superblock;
pub type Ext2Inode = Inode;

// ---- Core filesystem handle ----

pub struct Ext2Fs<'a> {
    device: &'a dyn BlockDevice,
    superblock: Superblock,
    /// Borrowed handle to the long-lived, persistent buffer cache. The cache
    /// is owned by the mounted-filesystem state (`ext2_vfs::CachedExt2`) and
    /// survives across operations; this struct is a cheap, allocation-free
    /// per-call view over it.
    cache: &'a mut BlockCache,
    block_size: u32,
    inode_size: u16,
    ptrs_per_block: u32,
    /// Set when an operation changed the in-memory superblock free-counts so
    /// the on-disk superblock is now stale. Persisted in ordered fashion by
    /// [`Self::sync`]; never written mid-operation (see the ordered-writeback
    /// rationale on `sync`).
    superblock_dirty: bool,
}

impl<'a> Ext2Fs<'a> {
    /// Read and validate the on-disk superblock *without* a cache.
    ///
    /// Used at mount time to learn the geometry (`block_size`) required to size
    /// the persistent [`BlockCache`] before it exists, and by tests. Returns
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

    /// Construct a thin FS handle over an externally-owned, persistent
    /// [`BlockCache`]. **Performs no allocation** — the heavy cache lives in
    /// the long-lived FS state and is merely borrowed here, so building one of
    /// these per VFS call is free.
    pub fn new(
        device: &'a dyn BlockDevice,
        cache: &'a mut BlockCache,
        superblock: Superblock,
        block_size: u32,
        inode_size: u16,
    ) -> Self {
        Self {
            device,
            superblock,
            cache,
            block_size,
            inode_size,
            ptrs_per_block: block_size / 4,
            superblock_dirty: false,
        }
    }

    pub fn superblock(&self) -> Superblock {
        self.superblock
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Number of dirty (unwritten) blocks currently held by the shared cache.
    pub fn dirty_count(&self) -> usize {
        self.cache.dirty_count()
    }

    /// Flush all dirty cached blocks and the superblock to the backing device
    /// with **ordered durability barriers**.
    ///
    /// Ordering follows the ext2 `data=ordered` discipline (minus a metadata
    /// journal — see the crate-level notes):
    ///   1. write back all dirty *data* blocks;
    ///   2. [`BlockDevice::flush`] barrier — data durable before any metadata
    ///      that references it;
    ///   3. write back all dirty *metadata* blocks (bitmaps, group descriptors,
    ///      inode tables, directory blocks, indirect blocks);
    ///   4. barrier — metadata durable before the superblock accounting for it;
    ///   5. write the superblock free-counts if dirty;
    ///   6. final barrier.
    ///
    /// A crash between phases can leave recoverable free-count drift (fsck
    /// territory) but never a directory entry / inode pointing at
    /// uninitialised on-disk data.
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

    /// Backwards-compatible durability call: alias for [`Self::sync`].
    pub fn flush(&mut self) -> Result<(), Ext2Error> {
        self.sync()
    }

    fn device_barrier(&self) -> Result<(), Ext2Error> {
        self.device.flush().map_err(|_| Ext2Error::DeviceError)
    }

    // ---- Inode I/O ----

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

    // ---- File I/O ----

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
        // A write that allocated blocks changed the superblock free-count;
        // mark it for ordered persistence by `sync` (never written mid-op).
        if self.superblock.free_blocks_count != free_before {
            self.superblock_dirty = true;
        }
        result
    }

    // ---- Directory operations ----

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

    // ---- Create / delete ----

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
        if name.is_empty() || name.len() > 255 {
            return Err(Ext2Error::NameTooLong);
        }
        let mut parent = self.read_inode_num(parent_num)?;
        if !parent.is_directory() {
            return Err(Ext2Error::NotDirectory);
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
        // Superblock free-counts changed (inode + maybe a dir data block were
        // allocated). Defer the on-disk superblock write to `sync` so it lands
        // *after* the bitmap/inode-table blocks it accounts for (ordered
        // writeback); the in-memory superblock is already updated.
        self.superblock_dirty = true;

        Ok(new_ino)
    }

    pub fn unlink_entry(&mut self, parent: u32, name: &[u8]) -> Result<(), Ext2Error> {
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

        // Free data blocks
        self.release_file_blocks(&target)?;
        ext2_alloc::free_inode(
            target_num,
            &mut self.superblock,
            &mut *self.cache,
            self.device,
            self.block_size,
        )?;

        // Zero the inode on disk
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

        // Re-read parent in case dir ops mutated the cached block
        parent_inode = self.read_inode_num(parent_num)?;
        self.write_inode_num(parent_num, &parent_inode)?;
        // Freed inode + data blocks changed the superblock free-counts; defer
        // the on-disk write to ordered `sync`.
        self.superblock_dirty = true;

        Ok(())
    }

    pub fn remove_path(&mut self, path: &[u8]) -> Result<(), Ext2Error> {
        let (parent_path, name) = ondisk::split_parent(path).ok_or(Ext2Error::PathNotFound)?;
        let parent = self.resolve_path(parent_path)?;
        self.unlink_entry(parent, name)
    }

    // ---- Internal helpers ----

    fn read_group_desc(&mut self, group: GroupIdx) -> Result<GroupDesc, Ext2Error> {
        let table_start: u32 = if self.block_size == 1024 { 2 } else { 1 };
        let desc_per_block = self.block_size as usize / 32;
        let block_idx = group.raw() as usize / desc_per_block;
        let within = (group.raw() as usize % desc_per_block) * 32;
        let blk_num = BlockNum(table_start + block_idx as u32);
        let block = self.cache.get(blk_num, self.device)?;
        Ok(GroupDesc::parse(&block.data()[within..within + 32]))
    }

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
        if inode.block[12].is_valid() {
            let indirect = inode.block[12];
            // Heap-allocate the indirect-block pointer buffer.  An
            // inline `[BlockNum::ZERO; 1024]` would put ≈4 KiB on the
            // kernel stack here (dominating this function's frame and
            // leaving little headroom for callees on a 32 KiB task
            // stack), so route through `slopos_ostd::KVec` instead.
            let ptrs: slopos_ostd::KVec<BlockNum> = {
                let block = self.cache.get(indirect, self.device)?;
                let data = block.data();
                let count = self.block_size as usize / 4;
                let mut ptrs = slopos_ostd::KVec::<BlockNum>::zeroed(1024)
                    .map_err(|_| Ext2Error::OutOfMemory)?;
                for i in 0..count.min(1024) {
                    let off = i * 4;
                    ptrs[i] = BlockNum(u32::from_le_bytes([
                        data[off],
                        data[off + 1],
                        data[off + 2],
                        data[off + 3],
                    ]));
                }
                ptrs
            };
            for &p in ptrs.iter().take(self.block_size as usize / 4) {
                if p.is_valid() {
                    ext2_alloc::free_block(
                        p,
                        &mut self.superblock,
                        &mut *self.cache,
                        self.device,
                        self.block_size,
                    )?;
                }
            }
            ext2_alloc::free_block(
                indirect,
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
