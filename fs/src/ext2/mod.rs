pub mod blockmap;
pub mod cache;
pub mod dir;
#[path = "alloc.rs"]
pub mod ext2_alloc;
pub mod file;
pub mod geometry;
pub mod inode;
pub mod ondisk;
pub mod symlink;
pub mod time;
pub mod types;

use cache::{BlockCache, BlockOwner};
use geometry::Ext2Geometry;
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
    geom: Ext2Geometry,
    cache: &'a mut BlockCache,
    block_size: u32,
    inode_size: u16,
    ptrs_per_block: u32,
    /// Set when an op changed the in-memory free-counts, leaving the on-disk
    /// superblock stale; persisted only by [`Self::sync`], never mid-operation.
    superblock_dirty: bool,
    /// Refuses every mutating entry point. Set when the image declares a
    /// read-only-compatible feature this implementation does not write, or
    /// when the device itself refuses writes (a verity-attested image).
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
    ) -> Result<Self, Ext2Error> {
        let read_only = Self::read_only_for(&superblock, device);
        let geom = Ext2Geometry::derive(&superblock)?;
        Ok(Self {
            device,
            superblock,
            geom,
            cache,
            block_size,
            inode_size,
            ptrs_per_block: block_size / 4,
            superblock_dirty: false,
            read_only,
        })
    }

    pub fn geometry(&self) -> &Ext2Geometry {
        &self.geom
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The one rule for whether a handle over `device` may mutate.
    pub fn read_only_for(superblock: &Superblock, device: &dyn BlockDevice) -> bool {
        superblock.requires_readonly() || device.write_protected()
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

    /// Barriered on the spot rather than left to a later `sync`: this is a
    /// sub-block write straight to the device, so it dirties nothing and is
    /// invisible to the cache's own accounting. The clean stamp in particular
    /// is the last write before power-off, and one left in a volatile device
    /// cache means an orderly shutdown still reads as a crash.
    fn write_superblock_state(&mut self) -> Result<(), Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        self.device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        sb_buf[58..60].copy_from_slice(&self.superblock.state.to_le_bytes());
        self.device
            .write_at(1024, &sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        self.device_barrier()
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

    /// A caller deciding whether a sync has work must consult this as well as
    /// [`Self::dirty_count`], or a clean cache reads as a durable one.
    pub fn unbarriered_writes(&self) -> usize {
        self.cache.unbarriered_writes()
    }

    /// Write dirty *data* blocks back with no barrier, as eviction does, while
    /// leaving metadata dirty — the state a durability test must be able to
    /// construct on purpose. Data only, because evicting the inode record too
    /// would leave nothing for the ordering barrier to order.
    #[cfg(feature = "tests")]
    pub fn cache_evict_data_for_test(&mut self) -> Result<(), Ext2Error> {
        self.cache
            .flush_where(self.device, |kind, _| kind == cache::BlockKind::Data)
            .map(|_| ())
    }

    /// Commit one inode: its data blocks, the allocation state that makes them
    /// reachable, and its on-disk record.
    ///
    /// Two things are deliberately outside the scope. The directory entry
    /// naming the inode is not committed: POSIX says an `fsync` on the parent
    /// directory does that, and a crash before the entry lands leaves an
    /// orphan inode, not a corrupt file. Nor is the superblock's free-count
    /// drift, which `e2fsck` recomputes and which a crash already mandates a
    /// check for (the mount stamped `EXT2_ERROR_FS`).
    ///
    /// Ordering follows `data=ordered` narrowed to one inode: data and
    /// allocation blocks, a barrier, then the inode-table block. A crash
    /// between the two leaves a record whose size predates the data, never one
    /// whose blocks hold a previous file's contents. Allocation state goes in
    /// the *first* phase because an inode published while the bitmap still
    /// calls its blocks free is an invitation to hand them to a second file.
    ///
    /// `data_only` currently selects nothing: an ext2 record carries the block
    /// pointers, the size and the timestamps in one 128-byte struct, so no
    /// write commits the first two without the third. It divides only once a
    /// timestamp alone can dirty a record, which needs the wall clock.
    pub fn sync_inode(&mut self, ino: u32, data_only: bool) -> Result<(), Ext2Error> {
        let _ = data_only;
        let ino_num = InodeNum(ino);
        // Proves the number lands inside a group's inode table before anything
        // is written on its behalf.
        let (table_block, _) = self.inode_disk_offset(ino_num)?;

        let (first, last) = self.inode_table_span(ino_num, table_block)?;

        // The record shares its block with its neighbours, so writing it
        // publishes their cached state too. Their data goes out in the same
        // phase as this inode's, or the barrier below would order a metadata
        // write ahead of data it already names.
        self.cache
            .flush_where(self.device, |_, owner| match owner {
                BlockOwner::File(owned) => owned >= first && owned <= last,
                BlockOwner::Alloc => true,
                BlockOwner::Inodes { .. } | BlockOwner::Other => false,
            })?;

        // Eviction may already have written this inode's data with no barrier
        // behind it, so what must be ordered is the device's commits, not this
        // function's writes.
        if self.cache.unbarriered_writes() > 0 {
            self.device_barrier()?;
        }
        self.cache.flush_block(table_block, self.device)?;
        if self.cache.unbarriered_writes() == 0 {
            return Ok(());
        }
        self.device_barrier()
    }

    /// Inclusive range of inode numbers whose records share `table_block`.
    ///
    /// Widened by one record on each side when `inode_size` does not divide
    /// the block size, because then a record straddles the boundary. Erring
    /// wide costs a few extra data blocks in the pre-flush; erring narrow
    /// would publish a neighbour's record ahead of its data.
    fn inode_table_span(
        &mut self,
        ino: InodeNum,
        table_block: BlockNum,
    ) -> Result<(u32, u32), Ext2Error> {
        let (group, _) = self.geom.locate_inode(ino).ok_or(Ext2Error::InvalidInode)?;
        let table_start = self
            .read_group_desc(group)?
            .inode_table
            .to_disk_offset(self.block_size)
            .raw();
        let block_start = table_block.to_disk_offset(self.block_size).raw();
        let into_table = block_start
            .checked_sub(table_start)
            .ok_or(Ext2Error::InvalidInode)?;

        let inode_size = self.inode_size.max(1) as u64;
        let per_block = (self.block_size as u64).div_ceil(inode_size);
        let first_local = (into_table / inode_size) as u32;
        let aligned = self.block_size as u64 % inode_size == 0;
        let slack = u32::from(!aligned);

        // Saturating throughout: the inputs are superblock-derived, so a
        // hostile image must widen the span (which only costs a larger
        // pre-flush) rather than overflow.
        let per_group = self.geom.inodes_per_group();
        let base = group
            .raw()
            .saturating_mul(per_group)
            .saturating_add(1)
            .min(self.geom.inodes_count());
        let group_last = base
            .saturating_add(per_group.saturating_sub(1))
            .min(self.geom.inodes_count());
        let first_in_block = base.saturating_add(first_local);
        let first = first_in_block.saturating_sub(slack).max(base);
        let last = first_in_block
            .saturating_add(per_block as u32)
            .saturating_sub(1)
            .saturating_add(slack)
            .min(group_last);
        Ok((first, last.max(first)))
    }

    /// Ordered durability, following the ext2 `data=ordered` discipline (minus a
    /// metadata journal): data blocks, barrier, metadata blocks, barrier,
    /// superblock free-counts, barrier. A crash between phases can leave
    /// recoverable free-count drift but never a directory entry or inode
    /// pointing at uninitialised on-disk data.
    pub fn sync(&mut self) -> Result<(), Ext2Error> {
        // A barrier over nothing orders nothing, and this runs on the flusher's
        // five-second tick and on every unprivileged `sync(2)`.
        if self.cache.dirty_count() == 0
            && self.cache.unbarriered_writes() == 0
            && !self.superblock_dirty
        {
            return Ok(());
        }
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

    fn device_barrier(&mut self) -> Result<(), Ext2Error> {
        self.device.flush().map_err(|_| Ext2Error::DeviceError)?;
        self.cache.note_barrier();
        Ok(())
    }

    pub fn read_inode(&mut self, ino: u32) -> Result<Inode, Ext2Error> {
        self.read_inode_num(InodeNum(ino))
    }

    /// Byte offset of an inode inside the image, proven to lie within the
    /// group's inode table rather than derived from an unchecked descriptor.
    fn inode_disk_offset(&mut self, ino: InodeNum) -> Result<(BlockNum, usize), Ext2Error> {
        let (group, local) = self.geom.locate_inode(ino).ok_or(Ext2Error::InvalidInode)?;
        let desc = self.read_group_desc(group)?;
        if !desc.inode_table.is_valid() {
            return Err(Ext2Error::InvalidInode);
        }
        let byte_off = desc.inode_table.to_disk_offset(self.block_size).raw()
            + (local as u64 * self.inode_size as u64);
        let blk_raw = u32::try_from(byte_off / self.block_size as u64)
            .map_err(|_| Ext2Error::InvalidInode)?;
        let blk_num = self
            .geom
            .checked_block(blk_raw)
            .ok_or(Ext2Error::InvalidInode)?;
        let within = (byte_off % self.block_size as u64) as usize;
        if within + self.inode_size as usize > self.block_size as usize {
            return Err(Ext2Error::InvalidInode);
        }
        Ok((blk_num, within))
    }

    fn read_inode_num(&mut self, ino: InodeNum) -> Result<Inode, Ext2Error> {
        let (blk_num, within) = self.inode_disk_offset(ino)?;
        let size = self.inode_size as usize;
        let owner = self.inode_block_owner(ino, blk_num)?;
        let block = self.cache.get_owned(blk_num, self.device, owner)?;
        let data = block.data();
        if within + size > data.len() {
            return Err(Ext2Error::InvalidInode);
        }
        Ok(Inode::parse(&data[within..within + size]))
    }

    /// Classify an inode-table block by the records it carries, so a per-inode
    /// sync can tell it apart from a directory block. A span that fails to
    /// close degrades to [`BlockOwner::Other`], which is the conservative
    /// answer: `sync_inode` then declines to publish it early.
    fn inode_block_owner(
        &mut self,
        ino: InodeNum,
        table_block: BlockNum,
    ) -> Result<BlockOwner, Ext2Error> {
        let (first, last) = self.inode_table_span(ino, table_block)?;
        Ok(BlockOwner::Inodes { first, last })
    }

    fn write_inode_num(&mut self, ino: InodeNum, inode: &Inode) -> Result<(), Ext2Error> {
        let (blk_num, within) = self.inode_disk_offset(ino)?;
        let size = self.inode_size as usize;
        let owner = self.inode_block_owner(ino, blk_num)?;
        let mut block = self.cache.get_owned(blk_num, self.device, owner)?;
        let data = block.data_mut();
        if within + size > data.len() {
            return Err(Ext2Error::InvalidInode);
        }
        inode.encode(&mut data[within..within + size]);
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
            BlockOwner::File(ino),
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
            &self.geom,
            BlockOwner::File(ino),
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
            BlockOwner::File(ino),
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
                    BlockOwner::File(current.raw()),
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
                    BlockOwner::File(current.raw()),
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
            BlockOwner::File(parent_num.raw()),
        )
        .is_ok()
        {
            return Err(Ext2Error::AlreadyExists);
        }

        let parent_group = self
            .geom
            .locate_inode(parent_num)
            .map(|(g, _)| g)
            .ok_or(Ext2Error::InvalidInode)?;
        let new_ino = ext2_alloc::allocate_inode(
            parent_group,
            &self.geom,
            &mut self.superblock,
            &mut *self.cache,
            self.device,
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
                &self.geom,
                &mut self.superblock,
                &mut *self.cache,
                self.device,
            )?;
            {
                let mut blk = self.cache.get_zero_data(
                    first_block,
                    self.device,
                    BlockOwner::File(new_ino.raw()),
                )?;
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
            &self.geom,
            BlockOwner::File(parent_num.raw()),
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
            BlockOwner::File(parent),
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
            BlockOwner::File(parent),
        )?;

        self.release_file_blocks(&target, BlockOwner::File(target_num.raw()))?;
        ext2_alloc::free_inode(
            target_num,
            &self.geom,
            &mut self.superblock,
            &mut *self.cache,
            self.device,
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
        ext2_alloc::read_group_desc(group, &self.geom, self.cache, self.device)
    }

    /// Free every block an inode owns: the twelve direct ones and all three
    /// indirect trees. Missing depths 2 and 3 leaks every block past the
    /// single-indirect reach on each delete, with no way to recover the space
    /// short of reformatting.
    fn release_file_blocks(&mut self, inode: &Inode, owner: BlockOwner) -> Result<(), Ext2Error> {
        for blk in inode.block.iter().take(12) {
            if blk.is_valid() {
                ext2_alloc::free_block(
                    *blk,
                    &self.geom,
                    &mut self.superblock,
                    &mut *self.cache,
                    self.device,
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
                owner,
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
                &self.geom,
                &mut self.superblock,
                &mut *self.cache,
                self.device,
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
