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
    DIR_FT_DIR, DIR_FT_REG_FILE, DIR_FT_SYMLINK, DirEntry, EXT2_ERROR_FS, EXT2_IMMUTABLE_FL,
    EXT2_VALID_FS, GroupDesc, Inode, MODE_DIRECTORY, MODE_FILE, MODE_PERM_MASK,
    RO_COMPAT_LARGE_FILE, Superblock,
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
    /// The inode carries `EXT2_IMMUTABLE_FL` and refuses every mutation.
    Immutable,
    /// A rename would splice a directory into its own subtree, detaching it
    /// and everything under it from the root.
    InvalidPath,
}

pub type Ext2Superblock = Superblock;
pub type Ext2Inode = Inode;

/// What `create_inode_entry` is being asked to build.
#[derive(Copy, Clone)]
enum NewInode<'a> {
    File,
    Directory,
    Symlink(&'a [u8]),
}

/// Which of `unlink(2)` and `rmdir(2)` a removal is, so the wrong one on a
/// directory reports `EISDIR`/`ENOTDIR` instead of silently doing the other.
#[derive(Copy, Clone, PartialEq, Eq)]
enum RemoveKind {
    NonDirectory,
    Directory,
}

/// A validated rename, carrying only what the mutation stages need — not the
/// `Inode`s the validation read, which is what keeps each stage's frame small.
struct RenamePlan {
    source: InodeNum,
    source_is_dir: bool,
    file_type: u8,
    /// Set when the destination name exists and must be removed first.
    displaced: Option<RemoveKind>,
    /// The two ends name different directories, so a moved directory shifts a
    /// `..` link between them.
    reparenting: bool,
}

/// The directory-entry type byte matching an inode's mode. The `filetype`
/// incompat feature is negotiated, so an entry carrying the wrong byte is a
/// lookup that answers with the wrong `d_type` for every other reader.
fn dir_file_type(inode: &Inode) -> u8 {
    use ondisk::{
        DIR_FT_BLKDEV, DIR_FT_CHRDEV, DIR_FT_FIFO, DIR_FT_SOCK, DIR_FT_UNKNOWN, MODE_BLOCKDEV,
        MODE_CHARDEV, MODE_FIFO, MODE_SOCKET,
    };
    match inode.file_type_mode() {
        MODE_DIRECTORY => DIR_FT_DIR,
        MODE_FILE => DIR_FT_REG_FILE,
        ondisk::MODE_SYMLINK => DIR_FT_SYMLINK,
        MODE_CHARDEV => DIR_FT_CHRDEV,
        MODE_BLOCKDEV => DIR_FT_BLKDEV,
        MODE_FIFO => DIR_FT_FIFO,
        MODE_SOCKET => DIR_FT_SOCK,
        _ => DIR_FT_UNKNOWN,
    }
}

/// RAII scope for one all-or-nothing ext2 operation.
///
/// Rolls back on drop unless [`Self::commit`] ran, which covers the `?` exits
/// that make up most of the failure paths below as well as an explicit
/// `return Err`. The `Drop` is panic-free by construction: every step it takes
/// is a field assignment or a `BlockCache` method that cannot fail.
struct Ext2Txn<'t, 'a> {
    fs: &'t mut Ext2Fs<'a>,
    superblock: Superblock,
    superblock_dirty: bool,
    /// This scope opened the transaction, so its snapshot is the committed
    /// state and its rollback is the one that runs.
    outermost: bool,
    committed: bool,
}

impl<'t, 'a> Ext2Txn<'t, 'a> {
    fn begin(fs: &'t mut Ext2Fs<'a>) -> Self {
        // Only the outermost scope owns the superblock snapshot, matching
        // `BlockCache::begin_op`'s own depth rule. An inner scope that
        // restored its own snapshot while the outer one went on to commit
        // would leave the free counts disagreeing with the bitmaps the cache
        // still holds.
        let outermost = !fs.in_transaction;
        let superblock = fs.superblock;
        let superblock_dirty = fs.superblock_dirty;
        fs.in_transaction = true;
        fs.cache.begin_op();
        Self {
            fs,
            superblock,
            superblock_dirty,
            outermost,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
        self.fs.cache.commit_op();
    }
}

impl Drop for Ext2Txn<'_, '_> {
    fn drop(&mut self) {
        if self.committed {
            if self.outermost {
                self.fs.in_transaction = false;
            }
            return;
        }
        self.fs.cache.rollback_op();
        if self.outermost {
            self.fs.superblock = self.superblock;
            self.fs.superblock_dirty = self.superblock_dirty;
            self.fs.in_transaction = false;
        }
    }
}

/// Mutating entry points below carry `#[inline(never)]`. Each holds one or
/// two `Inode`s live along with a directory helper's own frame, and inlining
/// several into one caller sums past the 2 KiB stack gate.
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
    /// A [`Ext2Txn`] scope is open, so a nested one must not take a second
    /// superblock snapshot.
    in_transaction: bool,
}

impl<'a> Ext2Fs<'a> {
    /// Reads the superblock *without* a cache: mount needs `block_size` to size
    /// the [`BlockCache`] before it exists. Returns
    /// `(superblock, block_size, inode_size)`.
    ///
    /// `#[inline(never)]`, as with the other two superblock-I/O helpers: each
    /// stages the full 1024-byte block on its own frame, and three of those
    /// inlined into one caller is 3 KiB of the 2 KiB budget.
    #[inline(never)]
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
            in_transaction: false,
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

    /// Run `f` as one all-or-nothing operation: an error rolls back every
    /// block it dirtied and every free-count it moved, so a later flush cannot
    /// publish half of it.
    ///
    /// Every mutating entry point below is wrapped in one. What the rollback
    /// does *not* cover is a block the cache had to evict mid-operation, which
    /// reached the device before the failure; [`BlockCache::find_or_evict`]
    /// makes that the victim of last resort, and closing the residual needs a
    /// journal rather than a wider guard.
    fn transaction<R>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<R, Ext2Error>,
    ) -> Result<R, Ext2Error> {
        let mut txn = Ext2Txn::begin(self);
        let result = f(txn.fs);
        // A scope whose undo record overflowed can no longer be undone, so
        // committing it would publish work the guard can no longer stand
        // behind. Failing here rolls back what is still recorded, which is a
        // consistent prefix, and the caller sees `NoSpace`.
        if result.is_ok() && txn.fs.cache.op_undo_overflowed() {
            return Err(Ext2Error::NoSpace);
        }
        if result.is_ok() {
            txn.commit();
        }
        result
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
    #[inline(never)]
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
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<usize, Ext2Error> {
        let inode = self.read_inode_num(InodeNum(ino))?;
        file::read_file(
            &inode,
            offset,
            buffer,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(ino),
        )
    }

    #[inline(never)]
    pub fn write_file(&mut self, ino: u32, offset: u64, buffer: &[u8]) -> Result<usize, Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            let ino_num = InodeNum(ino);
            let mut inode = fs.read_inode_num(ino_num)?;
            if inode.is_immutable() {
                return Err(Ext2Error::Immutable);
            }
            let free_before = fs.superblock.free_blocks_count;
            // A short write still updated the size and kept its blocks, so the
            // inode record must be written whichever way this went; only a
            // zero-byte failure propagates as an error.
            let result = file::write_file(
                &mut inode,
                offset,
                buffer,
                &mut *fs.cache,
                fs.device,
                fs.ptrs_per_block,
                fs.block_size,
                &mut fs.superblock,
                &fs.geom,
                BlockOwner::File(ino),
            );
            let written = result?;
            time::stamp(&mut inode.mtime);
            time::stamp(&mut inode.ctime);
            fs.note_large_file(&inode);
            fs.write_inode_num(ino_num, &inode)?;
            if fs.superblock.free_blocks_count != free_before {
                fs.superblock_dirty = true;
            }
            Ok(written)
        })
    }

    /// Shrink or extend a regular file. Extension is sparse, following ext2:
    /// the hole reads as zeros and costs no blocks until it is written.
    #[inline(never)]
    pub fn truncate_file(&mut self, ino: u32, new_size: u64) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            let ino_num = InodeNum(ino);
            let mut inode = fs.read_inode_num(ino_num)?;
            if inode.is_immutable() {
                return Err(Ext2Error::Immutable);
            }
            if inode.is_directory() {
                return Err(Ext2Error::IsDirectory);
            }
            if !inode.is_regular_file() {
                return Err(Ext2Error::NotFile);
            }
            let free_before = fs.superblock.free_blocks_count;
            fs.release_blocks_past(&mut inode, new_size, BlockOwner::File(ino))?;
            time::stamp(&mut inode.mtime);
            time::stamp(&mut inode.ctime);
            fs.note_large_file(&inode);
            fs.write_inode_num(ino_num, &inode)?;
            if fs.superblock.free_blocks_count != free_before {
                fs.superblock_dirty = true;
            }
            fs.superblock_dirty = true;
            Ok(())
        })
    }

    /// A file above 4 GiB is only correctly read by an implementation that
    /// knows to consult `i_size_high`, and ext2 spells that as a
    /// read-only-compatible feature bit. Writing one without setting the bit
    /// hands every other reader a truncated size.
    fn note_large_file(&mut self, inode: &Inode) {
        if inode.needs_large_file_feature()
            && self.superblock.feature_ro_compat & RO_COMPAT_LARGE_FILE == 0
        {
            self.superblock.feature_ro_compat |= RO_COMPAT_LARGE_FILE;
            self.superblock_dirty = true;
        }
    }

    /// Free every block past `new_size` and update the inode's size and block
    /// count. Shared by `truncate_file` and directory removal.
    fn release_blocks_past(
        &mut self,
        inode: &mut Inode,
        new_size: u64,
        owner: BlockOwner,
    ) -> Result<(), Ext2Error> {
        let geom = self.geom;
        let superblock = &mut self.superblock;
        let cache = &mut *self.cache;
        let device = self.device;
        // `free_block` needs the same cache and superblock the walk borrows,
        // so the frees are collected and applied after it.
        let mut freed: slopos_ostd::KVec<BlockNum> = slopos_ostd::KVec::new();
        file::truncate(
            inode,
            new_size,
            cache,
            device,
            self.ptrs_per_block,
            self.block_size,
            owner,
            &mut |b| freed.push(b).map_err(|_| Ext2Error::OutOfMemory),
        )?;
        for blk in freed.iter() {
            ext2_alloc::free_block(*blk, &geom, superblock, cache, device)?;
        }
        Ok(())
    }

    /// Read a symlink's target into `buffer`, answering the byte count.
    #[inline(never)]
    pub fn read_symlink(&mut self, ino: u32, buffer: &mut [u8]) -> Result<usize, Ext2Error> {
        let inode = self.read_inode_num(InodeNum(ino))?;
        symlink::read_symlink(
            &inode,
            buffer,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(ino),
        )
    }

    /// Write `mode`'s permission bits through to `i_mode`, leaving the type
    /// nibble — a caller must not turn a file into a directory.
    #[inline(never)]
    pub fn set_mode(&mut self, ino: u32, mode: u16) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            let ino_num = InodeNum(ino);
            let mut inode = fs.read_inode_num(ino_num)?;
            if inode.is_immutable() {
                return Err(Ext2Error::Immutable);
            }
            inode.mode = (inode.mode & !MODE_PERM_MASK) | (mode & MODE_PERM_MASK);
            time::stamp(&mut inode.ctime);
            fs.write_inode_num(ino_num, &inode)
        })
    }

    /// Seal an inode with `EXT2_IMMUTABLE_FL`. One-way, as the VFS trait
    /// requires: nothing in this implementation clears the bit.
    #[inline(never)]
    pub fn set_sealed(&mut self, ino: u32) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            let ino_num = InodeNum(ino);
            let mut inode = fs.read_inode_num(ino_num)?;
            if inode.is_immutable() {
                return Ok(());
            }
            inode.flags |= EXT2_IMMUTABLE_FL;
            time::stamp(&mut inode.ctime);
            fs.write_inode_num(ino_num, &inode)
        })
    }

    pub fn is_sealed(&mut self, ino: u32) -> Result<bool, Ext2Error> {
        Ok(self.read_inode_num(InodeNum(ino))?.is_immutable())
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

    /// Walk a directory from a byte-offset cookie, handing the callback the
    /// cookie that resumes *after* each entry. Answers the cookie the walk
    /// reached, which equals the directory's size once it is exhausted.
    pub fn for_each_dir_entry_from<F>(
        &mut self,
        ino: u32,
        start: u64,
        mut f: F,
    ) -> Result<u64, Ext2Error>
    where
        F: FnMut(u64, DirEntry<'_>) -> bool,
    {
        let inode = self.read_inode_num(InodeNum(ino))?;
        dir::for_each_entry_from(
            &inode,
            start,
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

    #[inline(never)]
    pub fn create_file(&mut self, parent: u32, name: &[u8]) -> Result<u32, Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| fs.create_inode_entry(InodeNum(parent), name, NewInode::File))
            .map(|n| n.raw())
    }

    #[inline(never)]
    pub fn create_directory(&mut self, parent: u32, name: &[u8]) -> Result<u32, Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| fs.create_inode_entry(InodeNum(parent), name, NewInode::Directory))
            .map(|n| n.raw())
    }

    /// Create a symlink under `parent` pointing at `target`. Targets of 60
    /// bytes or fewer are fast symlinks, stored inline in `i_block`.
    pub fn create_symlink(
        &mut self,
        parent: u32,
        name: &[u8],
        target: &[u8],
    ) -> Result<u32, Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            fs.create_inode_entry(InodeNum(parent), name, NewInode::Symlink(target))
        })
        .map(|n| n.raw())
    }

    #[inline(never)]
    fn create_inode_entry(
        &mut self,
        parent_num: InodeNum,
        name: &[u8],
        kind: NewInode<'_>,
    ) -> Result<InodeNum, Ext2Error> {
        if name.is_empty() || name.len() > 255 {
            return Err(Ext2Error::NameTooLong);
        }
        if name == b"." || name == b".." {
            return Err(Ext2Error::AlreadyExists);
        }
        let mut parent = self.read_inode_num(parent_num)?;
        if !parent.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }
        if parent.is_immutable() {
            return Err(Ext2Error::Immutable);
        }
        let is_dir = matches!(kind, NewInode::Directory);
        // A directory's parent gains a `..` link, and `links_count` is a u16.
        if is_dir && parent.links_count == u16::MAX {
            return Err(Ext2Error::TooManyLinks);
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

        let mut new_inode = self.build_new_inode(new_ino, parent_num, kind)?;
        let now = time::now_unix();
        new_inode.atime = now;
        new_inode.ctime = now;
        new_inode.mtime = now;
        self.write_inode_num(new_ino, &new_inode)?;

        let ft = match kind {
            NewInode::Directory => DIR_FT_DIR,
            NewInode::File => DIR_FT_REG_FILE,
            NewInode::Symlink(_) => DIR_FT_SYMLINK,
        };
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
            self.adjust_used_dirs(new_ino, 1)?;
        }
        time::stamp(&mut parent.mtime);
        time::stamp(&mut parent.ctime);
        self.write_inode_num(parent_num, &parent)?;
        self.superblock_dirty = true;

        Ok(new_ino)
    }

    /// The in-memory record for a newly allocated inode, with the data blocks
    /// a directory or a slow symlink needs already written.
    #[inline(never)]
    fn build_new_inode(
        &mut self,
        new_ino: InodeNum,
        parent_num: InodeNum,
        kind: NewInode<'_>,
    ) -> Result<Inode, Ext2Error> {
        if let NewInode::Symlink(target) = kind {
            let data_block = if symlink::symlink_needs_block(target) {
                Some(ext2_alloc::allocate_block(
                    &self.geom,
                    &mut self.superblock,
                    &mut *self.cache,
                    self.device,
                )?)
            } else {
                None
            };
            return symlink::create_symlink_inode(
                target,
                self.block_size,
                data_block,
                &mut *self.cache,
                self.device,
                BlockOwner::File(new_ino.raw()),
            );
        }

        let is_dir = matches!(kind, NewInode::Directory);
        let mut inode = Inode {
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
        if !is_dir {
            return Ok(inode);
        }

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
        inode.block[0] = first_block;
        inode.size = self.block_size as u64;
        inode.blocks = self.block_size / 512;
        Ok(inode)
    }

    /// Move the `used_dirs_count` of the group holding `ino` by `delta`.
    /// `e2fsck` reports a stale one, and the allocator's directory-spreading
    /// heuristic is what the field exists for.
    fn adjust_used_dirs(&mut self, ino: InodeNum, delta: i32) -> Result<(), Ext2Error> {
        let (group, _) = self.geom.locate_inode(ino).ok_or(Ext2Error::InvalidInode)?;
        let mut desc = self.read_group_desc(group)?;
        desc.used_dirs_count = if delta >= 0 {
            desc.used_dirs_count.saturating_add(delta as u16)
        } else {
            desc.used_dirs_count.saturating_sub((-delta) as u16)
        };
        ext2_alloc::write_group_desc(group, &desc, &self.geom, &mut *self.cache, self.device)
    }

    #[inline(never)]
    pub fn unlink_entry(&mut self, parent: u32, name: &[u8]) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| fs.remove_entry(InodeNum(parent), name, RemoveKind::NonDirectory))
    }

    /// Remove an empty directory: `rmdir(2)`'s semantics, including the
    /// parent's `links_count` decrement for the vanished `..`.
    #[inline(never)]
    pub fn remove_directory(&mut self, parent: u32, name: &[u8]) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| fs.remove_entry(InodeNum(parent), name, RemoveKind::Directory))
    }

    #[inline(never)]
    fn remove_entry(
        &mut self,
        parent_num: InodeNum,
        name: &[u8],
        kind: RemoveKind,
    ) -> Result<(), Ext2Error> {
        if name == b"." || name == b".." {
            return Err(Ext2Error::NotEmpty);
        }
        let mut parent_inode = self.read_inode_num(parent_num)?;
        if !parent_inode.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }
        if parent_inode.is_immutable() {
            return Err(Ext2Error::Immutable);
        }

        let target_num = dir::lookup_child(
            &parent_inode,
            name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(parent_num.raw()),
        )?;
        let mut target = self.read_inode_num(target_num)?;
        if target.is_immutable() {
            return Err(Ext2Error::Immutable);
        }

        let is_dir = target.is_directory();
        match kind {
            RemoveKind::NonDirectory if is_dir => return Err(Ext2Error::IsDirectory),
            RemoveKind::Directory if !is_dir => return Err(Ext2Error::NotDirectory),
            _ => {}
        }
        if is_dir
            && !dir::is_dir_empty(
                &target,
                &mut *self.cache,
                self.device,
                self.ptrs_per_block,
                self.block_size,
                BlockOwner::File(target_num.raw()),
            )?
        {
            return Err(Ext2Error::NotEmpty);
        }

        dir::remove_dir_entry(
            &parent_inode,
            name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(parent_num.raw()),
        )?;

        // A name is not the inode. An image `mkfs` or any other kernel wrote
        // may carry several names for one inode, and freeing its blocks while
        // another name still points at them hands a live file's contents to
        // the next allocation. A directory is exempt: its two links are `.`
        // and its parent's entry, both of which this removal takes with it.
        let last_link = is_dir || target.links_count <= 1;
        if !last_link {
            target.links_count -= 1;
            time::stamp(&mut target.ctime);
            self.write_inode_num(target_num, &target)?;
        } else {
            // A fast symlink's target lives in `i_block`, which the block walk
            // would otherwise read as fifteen block numbers and free.
            if !target.is_fast_symlink() {
                self.release_file_blocks(&target, BlockOwner::File(target_num.raw()))?;
            }
            ext2_alloc::free_inode(
                target_num,
                &self.geom,
                &mut self.superblock,
                &mut *self.cache,
                self.device,
            )?;
            if is_dir {
                self.adjust_used_dirs(target_num, -1)?;
            }

            // `i_dtime` is what every other ext2 implementation stamps on a
            // free, and what `e2fsck` reads to tell a freed inode from a
            // corrupt one.
            target.mode = 0;
            target.links_count = 0;
            target.blocks = 0;
            target.size = 0;
            target.flags = 0;
            target.block = [BlockNum::ZERO; 15];
            target.dtime = time::now_unix();
            self.write_inode_num(target_num, &target)?;
        }

        // Re-read the parent: `remove_dir_entry` mutated its cached data
        // block, and `append_dir_entry` in an earlier op may have grown it.
        parent_inode = self.read_inode_num(parent_num)?;
        if is_dir {
            parent_inode.links_count = parent_inode.links_count.saturating_sub(1);
        }
        time::stamp(&mut parent_inode.mtime);
        time::stamp(&mut parent_inode.ctime);
        self.write_inode_num(parent_num, &parent_inode)?;
        self.superblock_dirty = true;

        Ok(())
    }

    /// Move `old_name` under `old_parent` to `new_name` under `new_parent`.
    ///
    /// The new entry is written before the old is removed, so a crash between
    /// the two leaves two names for one inode — recoverable by `e2fsck` —
    /// rather than none. That is the whole of the atomicity ext2 without a
    /// journal can offer.
    pub fn rename_entry(
        &mut self,
        old_parent: u32,
        old_name: &[u8],
        new_parent: u32,
        new_name: &[u8],
    ) -> Result<(), Ext2Error> {
        self.check_writable()?;
        self.transaction(|fs| {
            fs.rename_within(
                InodeNum(old_parent),
                old_name,
                InodeNum(new_parent),
                new_name,
            )
        })
    }

    /// Split into `#[inline(never)]` stages: each holds an `Inode` or two
    /// live, and one frame carrying all of them plus the inlined bodies of
    /// `remove_entry` and the directory helpers exceeds the 2 KiB stack cap.
    fn rename_within(
        &mut self,
        old_parent: InodeNum,
        old_name: &[u8],
        new_parent: InodeNum,
        new_name: &[u8],
    ) -> Result<(), Ext2Error> {
        if new_name.is_empty() || new_name.len() > 255 {
            return Err(Ext2Error::NameTooLong);
        }
        for name in [old_name, new_name] {
            if name == b"." || name == b".." {
                return Err(Ext2Error::InvalidPath);
            }
        }
        if old_parent == new_parent && old_name == new_name {
            return Ok(());
        }

        let Some(plan) = self.rename_plan(old_parent, old_name, new_parent, new_name)? else {
            return Ok(());
        };

        if let Some(kind) = plan.displaced {
            // POSIX: a directory may only be renamed over an *empty* one, and
            // that check lives in `remove_entry`, ahead of the removal.
            self.remove_entry(new_parent, new_name, kind)?;
        }

        self.rename_link_new(new_parent, new_name, &plan)?;
        self.rename_unlink_old(old_parent, old_name, &plan)?;

        if plan.source_is_dir && old_parent != new_parent {
            self.rename_fix_dotdot(plan.source, new_parent)?;
        }

        let mut moved = self.read_inode_num(plan.source)?;
        time::stamp(&mut moved.ctime);
        self.write_inode_num(plan.source, &moved)?;
        self.superblock_dirty = true;

        Ok(())
    }

    /// Validate a rename and describe what it will do. `Ok(None)` means the
    /// rename is a no-op because both names already denote one inode.
    #[inline(never)]
    fn rename_plan(
        &mut self,
        old_parent: InodeNum,
        old_name: &[u8],
        new_parent: InodeNum,
        new_name: &[u8],
    ) -> Result<Option<RenamePlan>, Ext2Error> {
        let source = self
            .lookup_in_dir(old_parent, old_name)?
            .ok_or(Ext2Error::PathNotFound)?;

        let (source_is_dir, file_type) = {
            let source_inode = self.read_inode_num(source)?;
            if source_inode.is_immutable() {
                return Err(Ext2Error::Immutable);
            }
            (source_inode.is_directory(), dir_file_type(&source_inode))
        };

        // Splicing a directory into its own subtree detaches it and everything
        // under it: unreachable from the root, and unremovable because the
        // walk that would find it starts there.
        if source_is_dir && (source == new_parent || self.is_ancestor(source, new_parent)?) {
            return Err(Ext2Error::InvalidPath);
        }

        let existing = self.lookup_in_dir(new_parent, new_name)?;

        let mut displaced = None;
        if let Some(existing) = existing {
            if existing == source {
                return Ok(None);
            }
            let existing_is_dir = self.read_inode_num(existing)?.is_directory();
            match (source_is_dir, existing_is_dir) {
                (true, false) => return Err(Ext2Error::NotDirectory),
                (false, true) => return Err(Ext2Error::IsDirectory),
                _ => {}
            }
            displaced = Some(if existing_is_dir {
                RemoveKind::Directory
            } else {
                RemoveKind::NonDirectory
            });
        }

        Ok(Some(RenamePlan {
            source,
            source_is_dir,
            file_type,
            displaced,
            reparenting: old_parent != new_parent,
        }))
    }

    /// Look a name up in a directory that must be one, and must not be sealed
    /// — the two checks every mutation through a parent needs, kept in one
    /// frame so the caller does not hold the parent `Inode` live across the
    /// rest of its work.
    #[inline(never)]
    fn lookup_in_dir(
        &mut self,
        parent: InodeNum,
        name: &[u8],
    ) -> Result<Option<InodeNum>, Ext2Error> {
        let parent_inode = self.read_inode_num(parent)?;
        if !parent_inode.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }
        if parent_inode.is_immutable() {
            return Err(Ext2Error::Immutable);
        }
        Ok(dir::lookup_child(
            &parent_inode,
            name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(parent.raw()),
        )
        .ok())
    }

    /// Write the new entry. Deliberately before the old one is removed: a
    /// crash here leaves two names for one inode, which `e2fsck` reconciles,
    /// where the other order would leave none and lose the file.
    #[inline(never)]
    fn rename_link_new(
        &mut self,
        new_parent: InodeNum,
        new_name: &[u8],
        plan: &RenamePlan,
    ) -> Result<(), Ext2Error> {
        let mut target_parent = self.read_inode_num(new_parent)?;
        if plan.source_is_dir && plan.reparenting && target_parent.links_count == u16::MAX {
            return Err(Ext2Error::TooManyLinks);
        }
        dir::append_dir_entry(
            &mut target_parent,
            plan.source,
            new_name,
            plan.file_type,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            &mut self.superblock,
            &self.geom,
            BlockOwner::File(new_parent.raw()),
        )?;
        if plan.source_is_dir && plan.reparenting {
            target_parent.links_count += 1;
        }
        time::stamp(&mut target_parent.mtime);
        time::stamp(&mut target_parent.ctime);
        self.write_inode_num(new_parent, &target_parent)
    }

    #[inline(never)]
    fn rename_unlink_old(
        &mut self,
        old_parent: InodeNum,
        old_name: &[u8],
        plan: &RenamePlan,
    ) -> Result<(), Ext2Error> {
        // Re-read: `append_dir_entry` may have grown the shared parent when
        // both ends name one directory.
        let source_parent = self.read_inode_num(old_parent)?;
        dir::remove_dir_entry(
            &source_parent,
            old_name,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(old_parent.raw()),
        )?;
        let mut source_parent = self.read_inode_num(old_parent)?;
        if plan.source_is_dir && plan.reparenting {
            source_parent.links_count = source_parent.links_count.saturating_sub(1);
        }
        time::stamp(&mut source_parent.mtime);
        time::stamp(&mut source_parent.ctime);
        self.write_inode_num(old_parent, &source_parent)
    }

    #[inline(never)]
    fn rename_fix_dotdot(
        &mut self,
        moved: InodeNum,
        new_parent: InodeNum,
    ) -> Result<(), Ext2Error> {
        let inode = self.read_inode_num(moved)?;
        dir::update_dotdot(
            &inode,
            new_parent,
            &mut *self.cache,
            self.device,
            self.ptrs_per_block,
            self.block_size,
            BlockOwner::File(moved.raw()),
        )
    }

    /// Whether `ancestor` is on the `..` chain above `descendant`. Bounded by
    /// the inode count, so a `..` cycle in a damaged image terminates.
    fn is_ancestor(&mut self, ancestor: InodeNum, descendant: InodeNum) -> Result<bool, Ext2Error> {
        let mut current = descendant;
        let mut steps = 0u32;
        let limit = self.geom.inodes_count();
        while current != InodeNum::ROOT {
            if current == ancestor {
                return Ok(true);
            }
            steps += 1;
            if steps > limit {
                return Err(Ext2Error::DirectoryFormat);
            }
            let inode = self.read_inode_num(current)?;
            let parent = dir::lookup_child(
                &inode,
                b"..",
                &mut *self.cache,
                self.device,
                self.ptrs_per_block,
                self.block_size,
                BlockOwner::File(current.raw()),
            )?;
            if parent == current {
                break;
            }
            current = parent;
        }
        Ok(current == ancestor)
    }

    pub fn remove_path(&mut self, path: &[u8]) -> Result<(), Ext2Error> {
        let (parent_path, name) = ondisk::split_parent(path).ok_or(Ext2Error::PathNotFound)?;
        let parent = self.resolve_path(parent_path)?;
        self.unlink_entry(parent, name)
    }

    fn read_group_desc(&mut self, group: GroupIdx) -> Result<GroupDesc, Ext2Error> {
        ext2_alloc::read_group_desc(group, &self.geom, self.cache, self.device)
    }

    /// Write an inode record verbatim, so a test can construct on-disk states
    /// this implementation has no operation for — a second hard link, above
    /// all, which every other ext2 writer produces and `unlink` must honour.
    #[cfg(feature = "tests")]
    pub fn write_inode_for_test(&mut self, ino: u32, inode: &Inode) -> Result<(), Ext2Error> {
        self.write_inode_num(InodeNum(ino), inode)
    }

    /// A group's directory count, which `e2fsck` cross-checks against the
    /// inodes it finds.
    pub fn group_used_dirs(&mut self, group: u32) -> Result<u16, Ext2Error> {
        let group = self.geom.group(group).ok_or(Ext2Error::InvalidBlock)?;
        Ok(self.read_group_desc(group)?.used_dirs_count)
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

    #[inline(never)]
    fn write_superblock(&mut self) -> Result<(), Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        self.device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        self.superblock.encode_mutable_fields(&mut sb_buf);
        self.device
            .write_at(1024, &sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        Ok(())
    }
}
