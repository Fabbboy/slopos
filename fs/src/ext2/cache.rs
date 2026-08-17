use slopos_ostd::mm::frame::{Frame, PageCacheMeta};
use slopos_ostd::{KBTreeMap, KVec};

use super::Ext2Error;
use super::ondisk::EXT2_MAX_BLOCK_SIZE;
use super::types::BlockNum;
use crate::blockdev::BlockDevice;

const CACHE_ENTRIES: usize = 128;

/// Whether a cached block holds file *data* or filesystem *metadata*
/// (bitmaps, group descriptors, inode tables, directory blocks, indirect
/// pointer blocks). The distinction drives ordered writeback: in the
/// crash-consistency model the FS aims for (ext2 `data=ordered`), data blocks
/// must reach stable storage *before* the metadata that references them, so a
/// crash can never expose a freshly-allocated inode/dir-entry pointing at
/// stale or uninitialised disk contents.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Data,
    Metadata,
}

/// One cache slot. The 4 KiB backing storage lives in
/// `frame: Frame<PageCacheMeta>` (HHDM-mapped, returned to the buddy
/// allocator when the slot drops). The dirty bit and the owner-key
/// backref are stored on the frame's typed metadata. `pinned`, `lru`,
/// `valid`, and `kind` are host-side bookkeeping.
struct CacheEntry {
    block: BlockNum,
    frame: Frame<PageCacheMeta>,
    pinned: u16,
    lru: u64,
    valid: bool,
    kind: BlockKind,
}

impl CacheEntry {
    fn new() -> Result<Self, Ext2Error> {
        let frame = Frame::<PageCacheMeta>::alloc().ok_or(Ext2Error::OutOfMemory)?;
        Ok(Self {
            block: BlockNum::ZERO,
            frame,
            pinned: 0,
            lru: 0,
            valid: false,
            kind: BlockKind::Metadata,
        })
    }
}

/// LRU-ordered, fixed-capacity, **write-back** block cache backed by
/// [`Frame<PageCacheMeta>`] pages from the buddy allocator.
///
/// # Lifecycle (gold-standard, persistent)
///
/// Unlike a per-operation scratch cache, one `BlockCache` is created at mount
/// time and **lives for the lifetime of the mounted filesystem**, owned by the
/// long-lived FS state (`ext2_vfs::CachedExt2`) and borrowed `&mut` by the
/// thin, per-call [`super::Ext2Fs`] handle. This is what makes it a real
/// buffer cache:
///
///   * dirty blocks accumulate across operations and are written back lazily
///     (on eviction, on explicit [`Self::flush_all`]/sync, or by the
///     background flusher) rather than synchronously per call;
///   * reads hit warm blocks across calls instead of re-reading the device;
///   * because the cache is never dropped, a forgotten flush can no longer
///     *lose* data — the bytes remain authoritative in the cache until
///     persisted. (The original data-loss bug was a *per-call* cache being
///     dropped unflushed.)
///
/// # Durability
///
/// `write_at` only guarantees the device *accepted* the bytes; on a write-back
/// device they may sit in a volatile cache. The FS issues
/// [`BlockDevice::flush`] barriers around ordered phases (see
/// `Ext2Fs::sync`). This cache never issues device flushes itself — it only
/// tracks dirtiness and orders its own writeback passes by [`BlockKind`].
pub struct BlockCache {
    entries: KVec<CacheEntry>,
    index: KBTreeMap<BlockNum, usize>,
    lru_clock: u64,
    block_size: u32,
}

impl BlockCache {
    pub fn new(block_size: u32) -> Result<Self, Ext2Error> {
        // Belt-and-braces: the slab callers already validate the
        // block size, but a stray construction with a larger size
        // would silently truncate sub-block reads.
        debug_assert!(block_size as usize <= EXT2_MAX_BLOCK_SIZE as usize);

        let mut entries = KVec::with_capacity(CACHE_ENTRIES).map_err(|_| Ext2Error::OutOfMemory)?;
        for _ in 0..CACHE_ENTRIES {
            // On the failing iteration `entries` drops with the
            // partial run already in flight; each in-flight
            // `Frame<PageCacheMeta>` returns its physical page to
            // the buddy via `PageCacheMeta::on_drop`.
            entries
                .push(CacheEntry::new()?)
                .map_err(|_| Ext2Error::OutOfMemory)?;
        }
        Ok(Self {
            entries,
            index: KBTreeMap::new(),
            lru_clock: 0,
            block_size,
        })
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Get a metadata block from the cache, reading from the device if not
    /// cached. The returned [`CachedBlock`] guard pins the slot.
    pub fn get<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<CachedBlock<'a>, Ext2Error> {
        self.get_kind(block, device, BlockKind::Metadata)
    }

    /// Get a file-data block from the cache (see [`BlockKind`]).
    pub fn get_data<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<CachedBlock<'a>, Ext2Error> {
        self.get_kind(block, device, BlockKind::Data)
    }

    fn get_kind<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        kind: BlockKind,
    ) -> Result<CachedBlock<'a>, Ext2Error> {
        if let Some(&slot) = self.index.get(&block) {
            self.lru_clock += 1;
            self.entries[slot].lru = self.lru_clock;
            self.entries[slot].pinned += 1;
            return Ok(CachedBlock { cache: self, slot });
        }

        let slot = self.find_or_evict(device)?;
        let offset = block.to_disk_offset(self.block_size);
        let bs = self.block_size as usize;
        {
            let entry = &mut self.entries[slot];
            device
                .read_at(offset.raw(), &mut entry.frame.as_bytes_mut()[..bs])
                .map_err(|_| Ext2Error::DeviceError)?;
        }

        self.lru_clock += 1;
        let entry = &mut self.entries[slot];
        entry.block = block;
        entry.kind = kind;
        entry.frame.set_owner_key(block.raw() as u64);
        entry.frame.set_dirty(false);
        entry.pinned = 1;
        entry.lru = self.lru_clock;
        entry.valid = true;
        self.index.insert(block, slot);

        Ok(CachedBlock { cache: self, slot })
    }

    /// Get a metadata block and zero-fill it (for newly allocated blocks — no
    /// disk read).
    pub fn get_zero(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        self.get_zero_kind(block, device, BlockKind::Metadata)
    }

    /// Get a file-data block and zero-fill it (newly allocated data block).
    pub fn get_zero_data(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        self.get_zero_kind(block, device, BlockKind::Data)
    }

    fn get_zero_kind(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        kind: BlockKind,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        if let Some(&slot) = self.index.get(&block) {
            let bs = self.block_size as usize;
            self.entries[slot].frame.as_bytes_mut()[..bs].fill(0);
            self.entries[slot].frame.set_dirty(true);
            self.entries[slot].kind = kind;
            self.lru_clock += 1;
            self.entries[slot].lru = self.lru_clock;
            self.entries[slot].pinned += 1;
            return Ok(CachedBlock { cache: self, slot });
        }

        let slot = self.find_or_evict(device)?;
        let bs = self.block_size as usize;

        self.lru_clock += 1;
        let entry = &mut self.entries[slot];
        entry.frame.as_bytes_mut()[..bs].fill(0);
        entry.block = block;
        entry.kind = kind;
        entry.frame.set_owner_key(block.raw() as u64);
        entry.frame.set_dirty(true);
        entry.pinned = 1;
        entry.lru = self.lru_clock;
        entry.valid = true;
        self.index.insert(block, slot);

        Ok(CachedBlock { cache: self, slot })
    }

    /// Flush a specific block to disk if dirty.
    pub fn flush_block(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        if let Some(&slot) = self.index.get(&block) {
            let entry = &mut self.entries[slot];
            if entry.frame.dirty() {
                let offset = entry.block.to_disk_offset(self.block_size);
                let bs = self.block_size as usize;
                device
                    .write_at(offset.raw(), &entry.frame.as_bytes()[..bs])
                    .map_err(|_| Ext2Error::DeviceError)?;
                entry.frame.set_dirty(false);
            }
        }
        Ok(())
    }

    /// Write back every dirty block of one [`BlockKind`].
    ///
    /// Unlike a fail-fast loop, this attempts *every* dirty slot of the kind
    /// even if one device write fails, so a transient error on one block does
    /// not silently strand the rest in the volatile cache. The first error
    /// encountered is returned after the full pass; successfully written blocks
    /// are marked clean, failed ones stay dirty (and will be retried on the
    /// next flush). Returns the number of blocks actually written on success.
    pub fn flush_kind(
        &mut self,
        kind: BlockKind,
        device: &dyn BlockDevice,
    ) -> Result<usize, Ext2Error> {
        let bs = self.block_size as usize;
        let mut first_err: Option<Ext2Error> = None;
        let mut written = 0usize;
        for entry in &mut self.entries {
            if !(entry.valid && entry.kind == kind && entry.frame.dirty()) {
                continue;
            }
            let offset = entry.block.to_disk_offset(self.block_size);
            match device.write_at(offset.raw(), &entry.frame.as_bytes()[..bs]) {
                Ok(()) => {
                    entry.frame.set_dirty(false);
                    written += 1;
                }
                Err(_) => {
                    if first_err.is_none() {
                        first_err = Some(Ext2Error::DeviceError);
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(written),
        }
    }

    /// Flush all dirty blocks (data first, then metadata) without device
    /// barriers. Callers that need durability ordering (`Ext2Fs::sync`)
    /// interleave [`BlockDevice::flush`] between the phases instead of relying
    /// on this. Returns the total number of blocks written.
    pub fn flush_all(&mut self, device: &dyn BlockDevice) -> Result<usize, Ext2Error> {
        let data = self.flush_kind(BlockKind::Data, device)?;
        let meta = self.flush_kind(BlockKind::Metadata, device)?;
        Ok(data + meta)
    }

    /// Number of dirty (not-yet-written-back) blocks currently held.
    pub fn dirty_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.valid && e.frame.dirty())
            .count()
    }

    /// Invalidate a cached block (evict without writing).
    pub fn invalidate(&mut self, block: BlockNum) {
        if let Some(slot) = self.index.remove(&block) {
            let entry = &mut self.entries[slot];
            entry.valid = false;
            entry.pinned = 0;
            entry.frame.set_dirty(false);
            entry.frame.set_owner_key(0);
        }
    }

    /// Frames holding a clean, unpinned block — what [`shrink_clean`] could
    /// give back right now.
    ///
    /// [`shrink_clean`]: Self::shrink_clean
    pub fn reclaimable(&self) -> u32 {
        self.entries
            .iter()
            .filter(|e| e.pinned == 0 && !e.frame.dirty())
            .count() as u32
    }

    /// Drop up to `want` clean, unpinned cache entries, returning their frames
    /// to the buddy. Returns how many were released.
    ///
    /// **Clean only, and that is the whole safety argument.** A clean block's
    /// bytes are already on disk, so dropping it costs a re-read and cannot
    /// lose data; a dirty one would need a device write, which is I/O on a
    /// path that runs *because* memory is short. A pinned entry has a live
    /// `CachedBlock` guard borrowing it and is not ours to drop.
    ///
    /// The cache re-grows on demand: `get_kind` allocates a fresh entry when
    /// none is free, so shrinking costs re-reads rather than correctness.
    pub fn shrink_clean(&mut self, want: u32) -> u32 {
        if want == 0 {
            return 0;
        }
        let mut released = 0u32;
        // From the end, so a `swap_remove` only ever moves an entry that has
        // already been considered.
        let mut i = self.entries.len();
        while i > 0 && released < want {
            i -= 1;
            if self.entries[i].pinned != 0 || self.entries[i].frame.dirty() {
                continue;
            }
            // Drop this entry's index key *before* the removal, and repair the
            // key of whatever `swap_remove` moves into its place. Rebuilding
            // the whole index afterwards would be simpler and is wrong: it
            // needs `KBTreeMap::insert`, which allocates, on the path that
            // runs *because* allocation failed — and a failed re-insert would
            // leave a live cached block unreachable under its own number.
            //
            // `remove` and the in-place `get_mut` below allocate nothing.
            self.index.remove(&self.entries[i].block);
            let moved_from = self.entries.len() - 1;
            let moved = if moved_from != i {
                Some(self.entries[moved_from].block)
            } else {
                None
            };
            let removed_valid = self.entries[moved_from].valid;
            // Dropping the entry drops its `Frame<PageCacheMeta>`, which is
            // what actually returns the page.
            self.entries.swap_remove(i);
            if let Some(block) = moved
                && removed_valid
                && let Some(slot) = self.index.get_mut(&block)
            {
                *slot = i;
            }
            released += 1;
        }
        released
    }

    fn find_or_evict(&mut self, device: &dyn BlockDevice) -> Result<usize, Ext2Error> {
        // First pass: find an empty slot.
        for (i, entry) in self.entries.iter().enumerate() {
            if !entry.valid {
                return Ok(i);
            }
        }

        // Re-grow after a reclaim took entries away. Without this the cache
        // would stay permanently shrunk and every later miss would evict a
        // live block instead of using the capacity it is entitled to.
        if self.entries.len() < CACHE_ENTRIES
            && let Ok(entry) = CacheEntry::new()
            && self.entries.push(entry).is_ok()
        {
            return Ok(self.entries.len() - 1);
        }

        // Second pass: find LRU unpinned entry.
        let mut best_slot = None;
        let mut best_lru = u64::MAX;
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.pinned == 0 && entry.lru < best_lru {
                best_lru = entry.lru;
                best_slot = Some(i);
            }
        }

        let slot = best_slot.ok_or(Ext2Error::DeviceError)?;

        // Flush the victim if dirty. Eviction is a cache-replacement event, not
        // a durability point, so no device barrier is issued here — the
        // FS-level `sync` provides ordering/barriers when it matters.
        let victim = &self.entries[slot];
        if victim.frame.dirty() {
            let offset = victim.block.to_disk_offset(self.block_size);
            let bs = self.block_size as usize;
            device
                .write_at(offset.raw(), &victim.frame.as_bytes()[..bs])
                .map_err(|_| Ext2Error::DeviceError)?;
        }

        // Remove from index.
        self.index.remove(&self.entries[slot].block);
        let entry = &mut self.entries[slot];
        entry.valid = false;
        entry.frame.set_dirty(false);
        entry.frame.set_owner_key(0);

        Ok(slot)
    }
}

/// RAII guard for a cached block. Automatically unpins on drop.
pub struct CachedBlock<'a> {
    cache: &'a mut BlockCache,
    slot: usize,
}

impl<'a> CachedBlock<'a> {
    pub fn data(&self) -> &[u8] {
        let bs = self.cache.block_size as usize;
        &self.cache.entries[self.slot].frame.as_bytes()[..bs]
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        let bs = self.cache.block_size as usize;
        let entry = &mut self.cache.entries[self.slot];
        entry.frame.set_dirty(true);
        &mut entry.frame.as_bytes_mut()[..bs]
    }

    pub fn block_num(&self) -> BlockNum {
        self.cache.entries[self.slot].block
    }
}

impl Drop for CachedBlock<'_> {
    fn drop(&mut self) {
        self.cache.entries[self.slot].pinned =
            self.cache.entries[self.slot].pinned.saturating_sub(1);
    }
}
