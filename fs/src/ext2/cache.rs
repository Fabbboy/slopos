use slopos_ostd::mm::frame::{Frame, PageCacheMeta};
use slopos_ostd::{KBTreeMap, KVec};

use super::Ext2Error;
use super::ondisk::EXT2_MAX_BLOCK_SIZE;
use super::types::BlockNum;
use crate::blockdev::BlockDevice;

const CACHE_ENTRIES: usize = 128;

/// One cache slot. The 4 KiB backing storage lives in
/// `frame: Frame<PageCacheMeta>` (HHDM-mapped, returned to the buddy
/// allocator when the slot drops). The dirty bit and the owner-key
/// backref are stored on the frame's typed metadata. `pinned`, `lru`,
/// and `valid` are host-side bookkeeping that does not need to
/// survive eviction.
struct CacheEntry {
    block: BlockNum,
    frame: Frame<PageCacheMeta>,
    pinned: u16,
    lru: u64,
    valid: bool,
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
        })
    }
}

/// LRU-ordered, fixed-capacity block cache backed by
/// [`Frame<PageCacheMeta>`] pages from the buddy allocator. Each
/// slot's metadata carries the dirty bit and an owner-backref key
/// (the on-disk [`BlockNum`]) so background writeback or future
/// inode-keyed lookup paths can sample the state without touching
/// the cache's outer lock.
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

    /// Get a block from the cache, reading from the device if not
    /// cached. The returned [`CachedBlock`] guard pins the slot.
    pub fn get<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
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
        entry.frame.set_owner_key(block.raw() as u64);
        entry.frame.set_dirty(false);
        entry.pinned = 1;
        entry.lru = self.lru_clock;
        entry.valid = true;
        self.index.insert(block, slot);

        Ok(CachedBlock { cache: self, slot })
    }

    /// Get a block and zero-fill it (for newly allocated blocks — no
    /// disk read).
    pub fn get_zero(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        if let Some(&slot) = self.index.get(&block) {
            let bs = self.block_size as usize;
            self.entries[slot].frame.as_bytes_mut()[..bs].fill(0);
            self.entries[slot].frame.set_dirty(true);
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

    /// Flush all dirty blocks to disk.
    pub fn flush_all(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        for entry in &mut self.entries {
            if entry.valid && entry.frame.dirty() {
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

    fn find_or_evict(&mut self, device: &dyn BlockDevice) -> Result<usize, Ext2Error> {
        // First pass: find an empty slot.
        for (i, entry) in self.entries.iter().enumerate() {
            if !entry.valid {
                return Ok(i);
            }
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

        // Flush the victim if dirty.
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
