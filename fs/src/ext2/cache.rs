use slopos_ostd::mm::frame::{Frame, PageCacheMeta};
use slopos_ostd::{KBTreeMap, KVec};

use super::Ext2Error;
use super::ondisk::EXT2_MAX_BLOCK_SIZE;
use super::types::BlockNum;
use crate::blockdev::BlockDevice;

const CACHE_ENTRIES: usize = 128;

/// Drives ordered writeback (ext2 `data=ordered`): data blocks must reach
/// stable storage *before* the metadata that references them, so a crash cannot
/// expose a fresh inode or dir-entry pointing at stale contents.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Data,
    Metadata,
}

/// The `frame` is both the slot's storage and, through its typed metadata, the
/// dirty bit and the owner-key backref.
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

/// LRU-ordered, fixed-capacity, **write-back** block cache. One cache lives for
/// the lifetime of the mount, so dirty blocks accumulate across operations and
/// are written back on eviction, on [`Self::flush_all`], or by the background
/// flusher.
///
/// Never issues device flushes itself: the barriers around ordered phases are
/// `Ext2Fs::sync`'s.
pub struct BlockCache {
    entries: KVec<CacheEntry>,
    index: KBTreeMap<BlockNum, usize>,
    lru_clock: u64,
    block_size: u32,
}

impl BlockCache {
    pub fn new(block_size: u32) -> Result<Self, Ext2Error> {
        // Callers validate this; a larger size would silently truncate
        // sub-block reads.
        debug_assert!(block_size as usize <= EXT2_MAX_BLOCK_SIZE as usize);

        let mut entries = KVec::with_capacity(CACHE_ENTRIES).map_err(|_| Ext2Error::OutOfMemory)?;
        for _ in 0..CACHE_ENTRIES {
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

    /// Get a metadata block, reading from the device on a miss.
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

    /// Attempts every slot even after a write fails, returning the first error
    /// once the pass completes; failed blocks stay dirty for the next flush.
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

    /// Data first, then metadata, without device barriers: callers needing
    /// durability ordering interleave [`BlockDevice::flush`] between the phases.
    pub fn flush_all(&mut self, device: &dyn BlockDevice) -> Result<usize, Ext2Error> {
        let data = self.flush_kind(BlockKind::Data, device)?;
        let meta = self.flush_kind(BlockKind::Metadata, device)?;
        Ok(data + meta)
    }

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

    /// Frames holding a clean, unpinned block — what [`Self::shrink_clean`]
    /// could give back right now.
    pub fn reclaimable(&self) -> u32 {
        self.entries
            .iter()
            .filter(|e| e.pinned == 0 && !e.frame.dirty())
            .count() as u32
    }

    /// Drop up to `want` clean, unpinned entries, returning their frames to the
    /// buddy. Clean only: dropping one costs a re-read, whereas a dirty block
    /// would need a device write on a path that runs *because* memory is short.
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
            // Repair the index in place: rebuilding it needs
            // `KBTreeMap::insert`, which allocates, on the path that runs
            // *because* allocation failed.
            self.index.remove(&self.entries[i].block);
            let moved_from = self.entries.len() - 1;
            let moved = if moved_from != i {
                Some(self.entries[moved_from].block)
            } else {
                None
            };
            let removed_valid = self.entries[moved_from].valid;
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
        for (i, entry) in self.entries.iter().enumerate() {
            if !entry.valid {
                return Ok(i);
            }
        }

        // Re-grow after a reclaim took entries away: otherwise the cache stays
        // permanently shrunk and every later miss evicts a live block.
        if self.entries.len() < CACHE_ENTRIES
            && let Ok(entry) = CacheEntry::new()
            && self.entries.push(entry).is_ok()
        {
            return Ok(self.entries.len() - 1);
        }

        let mut best_slot = None;
        let mut best_lru = u64::MAX;
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.pinned == 0 && entry.lru < best_lru {
                best_lru = entry.lru;
                best_slot = Some(i);
            }
        }

        let slot = best_slot.ok_or(Ext2Error::DeviceError)?;

        // Eviction is a cache-replacement event, not a durability point, so no
        // device barrier is issued here; FS-level `sync` provides the ordering.
        let victim = &self.entries[slot];
        if victim.frame.dirty() {
            let offset = victim.block.to_disk_offset(self.block_size);
            let bs = self.block_size as usize;
            device
                .write_at(offset.raw(), &victim.frame.as_bytes()[..bs])
                .map_err(|_| Ext2Error::DeviceError)?;
        }

        self.index.remove(&self.entries[slot].block);
        let entry = &mut self.entries[slot];
        entry.valid = false;
        entry.frame.set_dirty(false);
        entry.frame.set_owner_key(0);

        Ok(slot)
    }
}

/// Pins its cache slot until dropped.
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
