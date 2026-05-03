use slopos_ostd::{KBTreeMap, KBox, KVec};

use super::Ext2Error;
use super::ondisk::EXT2_MAX_BLOCK_SIZE;
use super::types::BlockNum;
use crate::blockdev::BlockDevice;

const CACHE_ENTRIES: usize = 128;

struct CacheEntry {
    block: BlockNum,
    data: KBox<[u8; EXT2_MAX_BLOCK_SIZE as usize]>,
    dirty: bool,
    pinned: u16,
    lru: u64,
    valid: bool,
}

impl CacheEntry {
    fn new() -> Result<Self, Ext2Error> {
        Ok(Self {
            block: BlockNum::ZERO,
            // Heap-direct zeroed allocation: avoids materialising a
            // 4 KiB `[0u8; EXT2_MAX_BLOCK_SIZE]` rvalue on the stack.
            data: KBox::<[u8; EXT2_MAX_BLOCK_SIZE as usize]>::zeroed()
                .map_err(|_| Ext2Error::OutOfMemory)?,
            dirty: false,
            pinned: 0,
            lru: 0,
            valid: false,
        })
    }
}

pub struct BlockCache {
    entries: KVec<CacheEntry>,
    index: KBTreeMap<BlockNum, usize>,
    lru_clock: u64,
    block_size: u32,
}

impl BlockCache {
    pub fn new(block_size: u32) -> Result<Self, Ext2Error> {
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

    /// Get a block from the cache, reading from device if not cached.
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
        device
            .read_at(offset.raw(), &mut self.entries[slot].data[..bs])
            .map_err(|_| Ext2Error::DeviceError)?;

        self.lru_clock += 1;
        let entry = &mut self.entries[slot];
        entry.block = block;
        entry.dirty = false;
        entry.pinned = 1;
        entry.lru = self.lru_clock;
        entry.valid = true;
        self.index.insert(block, slot);

        Ok(CachedBlock { cache: self, slot })
    }

    /// Get a block and zero-fill it (for newly allocated blocks — no disk read).
    pub fn get_zero(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        if let Some(&slot) = self.index.get(&block) {
            // Already cached — zero it and mark dirty
            let bs = self.block_size as usize;
            self.entries[slot].data[..bs].fill(0);
            self.entries[slot].dirty = true;
            self.lru_clock += 1;
            self.entries[slot].lru = self.lru_clock;
            self.entries[slot].pinned += 1;
            return Ok(CachedBlock { cache: self, slot });
        }

        let slot = self.find_or_evict(device)?;
        let bs = self.block_size as usize;

        self.lru_clock += 1;
        let entry = &mut self.entries[slot];
        entry.data[..bs].fill(0);
        entry.block = block;
        entry.dirty = true;
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
            if self.entries[slot].dirty {
                let offset = block.to_disk_offset(self.block_size);
                let bs = self.block_size as usize;
                device
                    .write_at(offset.raw(), &self.entries[slot].data[..bs])
                    .map_err(|_| Ext2Error::DeviceError)?;
                self.entries[slot].dirty = false;
            }
        }
        Ok(())
    }

    /// Flush all dirty blocks to disk.
    pub fn flush_all(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        for entry in &mut self.entries {
            if entry.valid && entry.dirty {
                let offset = entry.block.to_disk_offset(self.block_size);
                let bs = self.block_size as usize;
                device
                    .write_at(offset.raw(), &entry.data[..bs])
                    .map_err(|_| Ext2Error::DeviceError)?;
                entry.dirty = false;
            }
        }
        Ok(())
    }

    /// Invalidate a cached block (evict without writing).
    pub fn invalidate(&mut self, block: BlockNum) {
        if let Some(slot) = self.index.remove(&block) {
            self.entries[slot].valid = false;
            self.entries[slot].pinned = 0;
        }
    }

    fn find_or_evict(&mut self, device: &dyn BlockDevice) -> Result<usize, Ext2Error> {
        // First pass: find an empty slot
        for (i, entry) in self.entries.iter().enumerate() {
            if !entry.valid {
                return Ok(i);
            }
        }

        // Second pass: find LRU unpinned entry
        let mut best_slot = None;
        let mut best_lru = u64::MAX;
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.pinned == 0 && entry.lru < best_lru {
                best_lru = entry.lru;
                best_slot = Some(i);
            }
        }

        let slot = best_slot.ok_or(Ext2Error::DeviceError)?;

        // Flush the victim if dirty
        let victim = &self.entries[slot];
        if victim.dirty {
            let offset = victim.block.to_disk_offset(self.block_size);
            let bs = self.block_size as usize;
            device
                .write_at(offset.raw(), &victim.data[..bs])
                .map_err(|_| Ext2Error::DeviceError)?;
        }

        // Remove from index
        self.index.remove(&self.entries[slot].block);
        self.entries[slot].valid = false;

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
        &self.cache.entries[self.slot].data[..bs]
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        let bs = self.cache.block_size as usize;
        self.cache.entries[self.slot].dirty = true;
        &mut self.cache.entries[self.slot].data[..bs]
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
