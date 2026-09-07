use slopos_ostd::mm::frame::{Frame, PageCacheMeta};
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota;
use slopos_ostd::{KBTreeMap, KBox, KVec};

use super::Ext2Error;
use super::journal::Journal;
use super::ondisk::EXT2_MAX_BLOCK_SIZE;
use super::types::BlockNum;
use crate::blockdev::BlockDevice;

const CACHE_ENTRIES: usize = 512;

/// Data blocks an operation may put *in* the log rather than write home and
/// barrier behind. A log block costs a write, the barrier it replaces costs a
/// device flush; the bound keeps a large write out of the log.
const DATA_LOG_LIMIT: usize = 16;

/// Snapshots one operation may hold. Each owns a block-sized copy, so this is
/// the ceiling on the rollback guard's memory: at a 4 KiB block size, 2 MiB.
///
/// Only a block that was *already dirty* when the operation first touched it
/// costs a record. A clean acquire — which is every block of a directory
/// scan, and every block a growing file allocates — costs one bit, so neither
/// directory size nor write size is bounded by this number.
const MAX_UNDO: usize = 512;

/// Drives ordered writeback (ext2 `data=ordered`): data blocks must reach
/// stable storage *before* the metadata that references them, so a crash cannot
/// expose a fresh inode or dir-entry pointing at stale contents.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Data,
    Metadata,
}

/// What a block's contents refer to, which is what decides whether a
/// *per-inode* writeback may publish it (see [`super::Ext2Fs::sync_inode`]).
/// Whole-filesystem [`super::Ext2Fs::sync`] ignores it and writes everything.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockOwner {
    /// Allocation state: block and inode bitmaps, group descriptors. Names no
    /// file contents, so writing one ahead of the rest can only leak space an
    /// `e2fsck` reclaims.
    Alloc,
    /// An inode-table block, carrying the block pointers of every inode in
    /// `first..=last`. `last` may over-reach the group; a wider span only
    /// makes the co-residency pre-flush write more than it must.
    Inodes {
        first: u32,
        last: u32,
    },
    File(u32),
    /// Everything else — directory blocks above all. A directory block names
    /// inodes, so publishing one before every inode table is on disk can
    /// resurrect a freed inode under a fresh name.
    Other,
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
    owner: BlockOwner,
    /// The open operation has touched this block, so it is both a rollback
    /// candidate and a victim of last resort (see [`BlockCache::find_or_evict`]).
    op_touched: bool,
    /// The open operation asked for this block to be dropped. Deferred to the
    /// commit — see [`BlockCache::invalidate`].
    op_invalidated: bool,
    /// The open operation found this block clean, so its rollback is to drop
    /// the entry and let the device's copy stand. A flag rather than an undo
    /// record: a clean block needs no snapshot, and recording one would put a
    /// block-sized allocation on every directory scan.
    op_discard: bool,
    /// Transient, set while [`BlockCache::rollback_op`] runs: this block's
    /// snapshot has been put back, so the discard pass must leave it alone.
    op_restored: bool,
    /// The operation that last dirtied this block. A writeback pass writes
    /// nothing newer than the epoch it fixed, which is what lets it release
    /// the mount lock between chunks.
    dirty_epoch: u64,
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
            owner: BlockOwner::Other,
            op_touched: false,
            op_invalidated: false,
            op_discard: false,
            op_restored: false,
            dirty_epoch: 0,
        })
    }
}

/// A block that was already dirty when the open operation first reached it.
///
/// The device holds *some earlier* state, so only this snapshot — taken
/// before the first mutation — is the committed one. The other case, a block
/// found clean, needs no record: the device already holds its committed
/// contents, so dropping the cache entry is the whole of the rollback, and
/// [`CacheEntry::op_discard`] says so in one bit instead of a block-sized
/// copy.
struct UndoEntry {
    block: BlockNum,
    snapshot: KVec<u8>,
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
    /// Blocks handed to the device since the last [`Self::note_barrier`].
    ///
    /// A clean cache is not a durable one: eviction writes back without a
    /// barrier by design, so `dirty_count() == 0` can hold over bytes still
    /// sitting in a write-back device cache. This is what lets a sync tell
    /// "nothing to do" from "nothing left to *write*".
    unbarriered: usize,
    /// Undo record of the open operation, empty when none is open.
    undo: KVec<UndoEntry>,
    /// Set when the open operation touched more blocks than [`MAX_UNDO`]
    /// permits. The scope can no longer be rolled back, so it must fail rather
    /// than commit half of itself.
    undo_overflow: bool,
    /// Nesting depth of [`Self::begin_op`]; only the outermost level records
    /// and only it rolls back.
    op_depth: u32,
    /// Counts operations, not writes, so a pass takes all of an operation's
    /// blocks or none of them.
    epoch: u64,
    /// The metadata redo log, when the image carries one. Owned here because
    /// every decision it changes — what an eviction may publish, what a
    /// rollback restores, where a miss reads from — is a cache decision.
    journal: Option<KBox<Journal>>,
    /// Block numbers staged for one log record. Preallocated so a commit
    /// allocates nothing.
    scratch: KVec<u32>,
    /// Who the open operation's block allocations are charged to.
    op_account: AccountId,
    /// Blocks the open operation has been charged for, refunded if it rolls
    /// back.
    op_charged: u32,
    /// Blocks the open operation freed, given back at the commit — a rollback
    /// restores the bitmap, so it still owes them.
    op_freed: u32,
    /// An eviction wrote one of the open operation's *data* blocks home, so
    /// the commit owes a barrier whichever path it takes: publishing metadata
    /// that names a home write still in a device cache is what
    /// `data=ordered` forbids.
    op_data_evicted: bool,
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
            unbarriered: 0,
            undo: KVec::new(),
            undo_overflow: false,
            op_depth: 0,
            epoch: 1,
            journal: None,
            scratch: KVec::new(),
            op_account: AccountId::NONE,
            op_charged: 0,
            op_freed: 0,
            op_data_evicted: false,
        })
    }

    /// The cache, on the heap — which every caller wants, because the 2 KiB
    /// stack gate refuses a frame carrying one alongside an `Ext2Fs`. Its own
    /// frame so the temporary this moves from is not the caller's.
    #[inline(never)]
    pub fn new_boxed(block_size: u32) -> Result<KBox<Self>, Ext2Error> {
        KBox::try_new(Self::new(block_size)?).map_err(|_| Ext2Error::OutOfMemory)
    }

    /// Hand the mount's redo log to the cache. Once installed, an operation's
    /// atomicity comes from the log rather than from undo snapshots.
    pub fn install_journal(&mut self, journal: KBox<Journal>) -> Result<(), Ext2Error> {
        // Sized to the cache, not to one record: an operation can dirty every
        // slot, and a commit must not allocate.
        self.scratch = KVec::with_capacity(CACHE_ENTRIES).map_err(|_| Ext2Error::OutOfMemory)?;
        self.journal = Some(journal);
        Ok(())
    }

    pub fn journal(&self) -> Option<&Journal> {
        self.journal.as_deref()
    }

    /// Whether the log has room for another operation without a check point
    /// first. `true` with no log at all: there is nothing to run out of.
    pub fn journal_has_headroom(&self) -> bool {
        self.journal.as_ref().is_none_or(|j| j.has_headroom())
    }

    /// Charge `blocks` to `account` for the open operation.
    ///
    /// Op-scoped because the transaction scope lives here: a rollback restores
    /// the bitmaps, so it owes the charge back too. `try_charge` takes no lock
    /// and allocates nothing, so this is legal under the mount lock.
    pub fn charge_blocks(&mut self, account: AccountId, blocks: u32) -> Result<(), Ext2Error> {
        if blocks == 0 {
            return Ok(());
        }
        quota::charge_blocks(account, blocks).map_err(|_| Ext2Error::NoSpace)?;
        self.op_account = account;
        self.op_charged = self.op_charged.saturating_add(blocks);
        Ok(())
    }

    /// Give `blocks` back at the commit rather than here: a rollback restores
    /// the bitmap, so an immediate refund would hand back a charge the
    /// operation still owes.
    pub fn note_blocks_freed(&mut self, account: AccountId, blocks: u32) {
        if blocks == 0 {
            return;
        }
        if self.op_depth == 0 {
            quota::refund_blocks(account, blocks);
            return;
        }
        self.op_account = account;
        self.op_freed = self.op_freed.saturating_add(blocks);
    }

    fn settle_charges(&mut self, committed: bool) {
        self.op_data_evicted = false;
        let (account, charged, freed) = (self.op_account, self.op_charged, self.op_freed);
        self.op_account = AccountId::NONE;
        self.op_charged = 0;
        self.op_freed = 0;
        if committed {
            quota::refund_blocks(account, freed);
        } else {
            quota::refund_blocks(account, charged);
        }
    }

    /// Whether the log has filled far enough that the flusher should drain it.
    /// `false` with no log: there is nothing to drain.
    pub fn journal_needs_drain(&self) -> bool {
        self.journal.as_ref().is_some_and(|j| j.needs_drain())
    }

    /// Note a block as freed, so no log record written before this point is
    /// ever replayed into it.
    pub fn note_revoke(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        let Some(mut journal) = self.journal.take() else {
            return Ok(());
        };
        let result = journal.note_revoke(block.raw(), device);
        self.unbarriered += journal.take_writes();
        self.journal = Some(journal);
        result
    }

    /// The epoch a writeback pass should fix. Blocks dirtied after it are a
    /// later operation's and are left for the next pass.
    pub fn writeback_epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether the open scope has outgrown its undo record. A `true` here is
    /// what turns an operation too large to undo into a refusal rather than a
    /// partial commit.
    pub fn op_undo_overflowed(&self) -> bool {
        self.undo_overflow
    }

    /// Open a rollback scope. Nested calls only count: an inner failure
    /// propagates outwards and the outermost scope is what rolls back, so a
    /// composite operation is undone as one.
    pub fn begin_op(&mut self) {
        self.op_depth += 1;
        if self.op_depth > 1 {
            return;
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.undo.clear();
        self.undo_overflow = false;
        for entry in &mut self.entries {
            entry.op_touched = false;
            entry.op_invalidated = false;
            entry.op_discard = false;
            entry.op_restored = false;
        }
        if let Some(journal) = self.journal.as_mut() {
            journal.begin_op();
        }
    }

    /// Accept every mutation the scope made.
    ///
    /// With a log this is where the operation becomes real, through
    /// [`Self::log_transaction`]. Without one the blocks merely stay dirty for
    /// the flusher, `sync`, or eviction to publish.
    pub fn commit_op(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if self.op_depth > 1 {
            self.op_depth -= 1;
            return Ok(());
        }
        // Before the flags are cleared: the log records are selected by them.
        self.log_transaction(device)?;
        self.op_depth = 0;
        self.settle_charges(true);
        self.undo.clear();
        self.undo_overflow = false;
        for i in 0..self.entries.len() {
            self.entries[i].op_touched = false;
            self.entries[i].op_discard = false;
            self.entries[i].op_restored = false;
            if !self.entries[i].op_invalidated {
                continue;
            }
            self.entries[i].op_invalidated = false;
            self.drop_entry(i);
        }
        Ok(())
    }

    /// Put every block the scope touched back the way it was, so nothing the
    /// failed operation wrote can reach the device.
    ///
    /// With a log the retraction needs no snapshots: no home block was written
    /// on the operation's behalf, so dropping every entry it touched is the
    /// whole of it, and a later read comes back from the log or from the
    /// block's home. Without one the scope is cache-deep — an eviction already
    /// put a touched block on the device, which is why
    /// [`Self::find_or_evict`] makes it the victim of last resort.
    pub fn rollback_op(&mut self) {
        self.op_depth = self.op_depth.saturating_sub(1);
        if self.op_depth > 0 {
            return;
        }
        if let Some(journal) = self.journal.as_mut() {
            journal.abort_op();
            for i in 0..self.entries.len() {
                if self.entries[i].op_touched {
                    self.drop_entry(i);
                }
            }
            self.clear_op_flags();
            return;
        }
        let bs = self.block_size as usize;
        let epoch = self.epoch;
        // Snapshots first. A block that was dirty on first touch has its
        // committed contents only here, so restoring must win over the
        // discard pass below, which a later eviction-and-re-acquire of the
        // same block would otherwise have flagged.
        while let Some(record) = self.undo.pop() {
            let Some(&slot) = self.index.get(&record.block) else {
                continue;
            };
            let entry = &mut self.entries[slot];
            let n = bs.min(record.snapshot.len());
            entry.frame.as_bytes_mut()[..n].copy_from_slice(&record.snapshot.as_slice()[..n]);
            entry.frame.set_dirty(true);
            entry.dirty_epoch = epoch;
            entry.op_restored = true;
        }
        for i in 0..self.entries.len() {
            if self.entries[i].op_discard && !self.entries[i].op_restored {
                self.drop_entry(i);
            }
        }
        self.clear_op_flags();
    }

    /// Also settles the operation's disk charges, which is why the rollback
    /// and commit paths both end here.
    fn clear_op_flags(&mut self) {
        self.settle_charges(false);
        self.undo_overflow = false;
        for entry in &mut self.entries {
            entry.op_touched = false;
            entry.op_discard = false;
            entry.op_restored = false;
            // The invalidations the operation asked for are undone with it:
            // the blocks it was freeing are still the inode's.
            entry.op_invalidated = false;
        }
    }

    /// Publish the open operation through the log.
    ///
    /// A small write's data goes *into* the log with its metadata, so replay
    /// restores both or neither and no ordering is needed. A large one writes
    /// home and buys the ordering with a barrier. Either way the barrier is
    /// issued only if a home write happened, so a `create` or an `unlink` pays
    /// nothing for it.
    fn log_transaction(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if self.journal.is_none() {
            return Ok(());
        }
        let bs = self.block_size as usize;
        // Staged and reserved *before* anything is published: a commit that
        // ran out of log room afterwards would retract the metadata with the
        // data already on the medium.
        let log_data = self.count_op_data() <= DATA_LOG_LIMIT;
        self.stage_blocks(log_data)?;
        let needed = self.log_slots_needed();
        if self
            .journal
            .as_ref()
            .is_some_and(|journal| journal.free_slots() < needed)
        {
            return Err(Ext2Error::NoSpace);
        }

        // Set by an eviction, so it is owed even on the path that writes no
        // data of its own.
        let mut wrote_data = self.op_data_evicted;
        if !log_data {
            for i in 0..self.entries.len() {
                let entry = &self.entries[i];
                if !(entry.valid
                    && entry.op_touched
                    && entry.frame.dirty()
                    && entry.kind == BlockKind::Data)
                {
                    continue;
                }
                let offset = entry.block.to_disk_offset(self.block_size);
                device
                    .write_at(offset.raw(), &entry.frame.as_bytes()[..bs])
                    .map_err(|_| Ext2Error::DeviceError)?;
                self.entries[i].frame.set_dirty(false);
                self.unbarriered += 1;
                wrote_data = true;
            }
        }
        if wrote_data {
            device.flush().map_err(|_| Ext2Error::DeviceError)?;
            self.unbarriered = 0;
        }

        let Some(mut journal) = self.journal.take() else {
            return Ok(());
        };
        let result = self.log_records(&mut journal, device);
        self.unbarriered += journal.take_writes();
        self.journal = Some(journal);
        result
    }

    /// Dirty data blocks the open operation touched.
    fn count_op_data(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.valid && e.op_touched && e.frame.dirty() && e.kind == BlockKind::Data)
            .count()
    }

    /// Collect the blocks the commit will log, metadata always and data when
    /// the caller decided to log it too.
    fn stage_blocks(&mut self, with_data: bool) -> Result<(), Ext2Error> {
        self.scratch.clear();
        for entry in &self.entries {
            let logged = match entry.kind {
                BlockKind::Metadata => true,
                BlockKind::Data => with_data,
            };
            if logged
                && entry.valid
                && entry.op_touched
                && entry.frame.dirty()
                && !entry.op_invalidated
            {
                self.scratch
                    .push(entry.block.raw())
                    .map_err(|_| Ext2Error::OutOfMemory)?;
            }
        }
        Ok(())
    }

    /// Slots the staged metadata plus its headers, the queued revokes and the
    /// commit record will take.
    fn log_slots_needed(&self) -> u32 {
        let Some(journal) = self.journal.as_ref() else {
            return 0;
        };
        let per_record = journal.max_entries().max(1);
        let payloads = self.scratch.len();
        let records = payloads.div_ceil(per_record);
        // One revoke record per full entry list, plus the commit record.
        let revokes = journal.queued_revokes().div_ceil(per_record);
        (payloads + records + revokes + 1) as u32
    }

    fn log_records(
        &mut self,
        journal: &mut Journal,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        journal.flush_revokes(device)?;
        if journal.op_is_empty() && self.scratch.is_empty() {
            return Ok(());
        }
        let bs = self.block_size as usize;
        let per_record = journal.max_entries();
        let mut done = 0usize;
        while done < self.scratch.len() {
            let take = (self.scratch.len() - done).min(per_record);
            let first =
                journal.begin_payloads(&self.scratch.as_slice()[done..done + take], device)?;
            for k in 0..take {
                let block = BlockNum(self.scratch.as_slice()[done + k]);
                let Some(&slot) = self.index.get(&block) else {
                    return Err(Ext2Error::DeviceError);
                };
                journal.write_payload(
                    first + k as u32,
                    &self.entries[slot].frame.as_bytes()[..bs],
                    device,
                )?;
            }
            done += take;
        }
        journal.write_commit(device)
    }

    /// Forget a slot's contents without writing them back.
    fn drop_entry(&mut self, slot: usize) {
        let block = self.entries[slot].block;
        let entry = &mut self.entries[slot];
        entry.valid = false;
        entry.pinned = 0;
        entry.frame.set_dirty(false);
        entry.frame.set_owner_key(0);
        self.index.remove(&block);
    }

    /// Record how `slot` is put back, before the caller can mutate it.
    ///
    /// Taken on *acquire* rather than on the first `data_mut`, because that
    /// accessor cannot fail and a snapshot allocates. Reads therefore record
    /// too, which costs one copy per distinct dirty metadata block an
    /// operation reaches — the alternative is a fallible mutable accessor at
    /// every call site and the same bound.
    ///
    /// Every fallible step happens before the entry is mutated, so a failure
    /// leaves the slot exactly as it was found. An entry that has been mutated
    /// with no undo record behind it is a block the rollback cannot see, which
    /// a later flush would publish over live data.
    fn note_op_touch(&mut self, slot: usize) -> Result<(), Ext2Error> {
        if self.op_depth == 0 || self.entries[slot].op_touched {
            return Ok(());
        }
        if self.journal.is_some() {
            // No home block carries this operation's changes, so the rollback
            // re-reads rather than restores: one bit, whatever the state.
            self.entries[slot].op_touched = true;
            return Ok(());
        }
        if !self.entries[slot].frame.dirty() {
            // Clean: the device holds the committed contents, so the rollback
            // is a drop and needs no snapshot.
            self.entries[slot].op_touched = true;
            self.entries[slot].op_discard = true;
            return Ok(());
        }
        if self.undo.len() >= MAX_UNDO {
            // An eviction clears `op_touched`, so a long operation can
            // re-acquire the same dirty bitmap and indirect blocks and record
            // each afresh. Refusing bounds the snapshot memory and turns the
            // excess into a failed operation the guard rolls back, rather than
            // an allocation storm that fails somewhere less recoverable.
            self.undo_overflow = true;
            return Err(Ext2Error::NoSpace);
        }
        let block = self.entries[slot].block;
        let bs = self.block_size as usize;
        let mut snapshot = KVec::<u8>::zeroed(bs).map_err(|_| Ext2Error::OutOfMemory)?;
        snapshot
            .as_mut_slice()
            .copy_from_slice(&self.entries[slot].frame.as_bytes()[..bs]);
        self.undo
            .push(UndoEntry { block, snapshot })
            .map_err(|_| Ext2Error::OutOfMemory)?;
        self.entries[slot].op_touched = true;
        Ok(())
    }

    /// Mark a freshly acquired block for discard on rollback.
    ///
    /// Infallible, which is what lets the acquire paths set it *after* the
    /// entry is installed: a block that was just read or zeroed has no
    /// committed cache state to preserve, so one bit is the whole record.
    fn note_op_fresh(&mut self, slot: usize) {
        if self.op_depth == 0 {
            return;
        }
        self.entries[slot].op_touched = true;
        self.entries[slot].op_discard = true;
    }

    pub fn unbarriered_writes(&self) -> usize {
        self.unbarriered
    }

    /// Call after issuing a [`BlockDevice::flush`].
    pub fn note_barrier(&mut self) {
        self.unbarriered = 0;
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
        self.get_kind(block, device, BlockKind::Metadata, BlockOwner::Other)
    }

    pub fn get_owned<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'a>, Ext2Error> {
        self.get_kind(block, device, BlockKind::Metadata, owner)
    }

    /// Get a file-data block from the cache (see [`BlockKind`]).
    pub fn get_data<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'a>, Ext2Error> {
        self.get_kind(block, device, BlockKind::Data, owner)
    }

    fn get_kind<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        kind: BlockKind,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'a>, Ext2Error> {
        if let Some(&slot) = self.index.get(&block) {
            self.note_op_touch(slot)?;
            self.lru_clock += 1;
            self.entries[slot].lru = self.lru_clock;
            self.entries[slot].pinned += 1;
            self.entries[slot].owner = owner;
            // Re-reached after a deferred invalidation: this operation is
            // using the block again, so the commit must not drop it.
            self.entries[slot].op_invalidated = false;
            return Ok(CachedBlock { cache: self, slot });
        }

        let slot = self.find_or_evict(device)?;
        let bs = self.block_size as usize;
        let home = block.to_disk_offset(self.block_size).raw();
        let staged = self.journal.take();
        let logged = staged.as_ref().and_then(|j| j.resident_slot(block.raw()));
        let read = {
            let entry = &mut self.entries[slot];
            let buffer = &mut entry.frame.as_bytes_mut()[..bs];
            match (logged, staged.as_ref()) {
                (Some(log_slot), Some(journal)) => journal.read_slot(log_slot, device, buffer),
                _ => device
                    .read_at(home, buffer)
                    .map_err(|_| Ext2Error::DeviceError),
            }
        };
        self.journal = staged;
        read?;

        self.lru_clock += 1;
        let epoch = self.epoch;
        let entry = &mut self.entries[slot];
        entry.block = block;
        entry.kind = kind;
        entry.owner = owner;
        entry.frame.set_owner_key(block.raw() as u64);
        // A block sourced from the log is *not* clean: its home holds the
        // state before the log's oldest uncheckpointed transaction, and a
        // clean entry would have the check point skip it and the reset drop
        // the only copy.
        entry.frame.set_dirty(logged.is_some());
        entry.dirty_epoch = epoch;
        entry.pinned = 1;
        entry.lru = self.lru_clock;
        entry.valid = true;
        entry.op_touched = false;
        entry.op_invalidated = false;
        entry.op_discard = false;
        entry.op_restored = false;
        self.index.insert(block, slot);
        // Read clean, so the rollback is a discard and needs no snapshot.
        self.note_op_fresh(slot);

        Ok(CachedBlock { cache: self, slot })
    }

    /// Get a metadata block and zero-fill it (for newly allocated blocks — no
    /// disk read).
    pub fn get_zero(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        self.get_zero_kind(block, device, BlockKind::Metadata, BlockOwner::Other)
    }

    pub fn get_zero_owned(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        self.get_zero_kind(block, device, BlockKind::Metadata, owner)
    }

    /// Get a file-data block and zero-fill it (newly allocated data block).
    pub fn get_zero_data(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        self.get_zero_kind(block, device, BlockKind::Data, owner)
    }

    fn get_zero_kind(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        kind: BlockKind,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        if let Some(&slot) = self.index.get(&block) {
            self.note_op_touch(slot)?;
            let bs = self.block_size as usize;
            self.entries[slot].frame.as_bytes_mut()[..bs].fill(0);
            self.entries[slot].frame.set_dirty(true);
            self.entries[slot].dirty_epoch = self.epoch;
            self.entries[slot].kind = kind;
            self.entries[slot].owner = owner;
            // A block re-reached after a deferred invalidation is being reused
            // by this same operation; dropping it at commit would throw the
            // reuse away.
            self.entries[slot].op_invalidated = false;
            self.lru_clock += 1;
            self.entries[slot].lru = self.lru_clock;
            self.entries[slot].pinned += 1;
            return Ok(CachedBlock { cache: self, slot });
        }

        let slot = self.find_or_evict(device)?;
        let bs = self.block_size as usize;
        let epoch = self.epoch;

        self.lru_clock += 1;
        let entry = &mut self.entries[slot];
        entry.frame.as_bytes_mut()[..bs].fill(0);
        entry.block = block;
        entry.kind = kind;
        entry.owner = owner;
        entry.frame.set_owner_key(block.raw() as u64);
        entry.frame.set_dirty(true);
        entry.dirty_epoch = epoch;
        entry.pinned = 1;
        entry.lru = self.lru_clock;
        entry.valid = true;
        entry.op_touched = false;
        entry.op_invalidated = false;
        entry.op_discard = false;
        entry.op_restored = false;
        self.index.insert(block, slot);
        // A freshly zeroed block has no committed contents to snapshot, so the
        // rollback is a discard whether or not the slot's predecessor was dirty.
        self.note_op_fresh(slot);

        Ok(CachedBlock { cache: self, slot })
    }

    /// Answers whether the block needed writing, so a caller can skip the
    /// device barrier that would otherwise order nothing.
    pub fn flush_block(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<bool, Ext2Error> {
        let Some(&slot) = self.index.get(&block) else {
            return Ok(false);
        };
        let entry = &mut self.entries[slot];
        if !entry.frame.dirty() {
            return Ok(false);
        }
        let offset = entry.block.to_disk_offset(self.block_size);
        let bs = self.block_size as usize;
        device
            .write_at(offset.raw(), &entry.frame.as_bytes()[..bs])
            .map_err(|_| Ext2Error::DeviceError)?;
        entry.frame.set_dirty(false);
        self.unbarriered += 1;
        Ok(true)
    }

    /// Attempts every slot even after a write fails, returning the first error
    /// once the pass completes; failed blocks stay dirty for the next flush.
    pub fn flush_where(
        &mut self,
        device: &dyn BlockDevice,
        select: impl FnMut(BlockKind, BlockOwner) -> bool,
    ) -> Result<usize, Ext2Error> {
        self.flush_bounded(device, u64::MAX, usize::MAX, select)
            .map(|progress| progress.written)
    }

    /// One bounded step of a writeback pass.
    ///
    /// `epoch` excludes blocks a later operation dirtied and `budget` caps the
    /// device writes, which together are what let the mount lock be released
    /// between steps: an operation that runs in the gap carries a newer epoch,
    /// so a resumed pass can neither miss its data nor publish its metadata
    /// early.
    pub fn flush_bounded(
        &mut self,
        device: &dyn BlockDevice,
        epoch: u64,
        budget: usize,
        mut select: impl FnMut(BlockKind, BlockOwner) -> bool,
    ) -> Result<FlushProgress, Ext2Error> {
        let bs = self.block_size as usize;
        let mut first_err: Option<Ext2Error> = None;
        let mut written = 0usize;
        let mut more = false;
        for entry in &mut self.entries {
            if !(entry.valid
                && entry.frame.dirty()
                && entry.dirty_epoch <= epoch
                && select(entry.kind, entry.owner))
            {
                continue;
            }
            if written >= budget {
                more = true;
                break;
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
        self.unbarriered += written;
        match first_err {
            Some(e) => Err(e),
            None => Ok(FlushProgress { written, more }),
        }
    }

    pub fn flush_kind(
        &mut self,
        kind: BlockKind,
        device: &dyn BlockDevice,
    ) -> Result<usize, Ext2Error> {
        self.flush_where(device, |entry_kind, _| entry_kind == kind)
    }

    /// Data first, then metadata, without device barriers: callers needing
    /// durability ordering interleave [`BlockDevice::flush`] between the phases.
    pub fn flush_all(&mut self, device: &dyn BlockDevice) -> Result<usize, Ext2Error> {
        let data = self.flush_kind(BlockKind::Data, device)?;
        let meta = self.flush_kind(BlockKind::Metadata, device)?;
        Ok(data + meta)
    }

    /// Copy home every logged block the cache no longer holds, from `cursor`
    /// onwards, and answer where to resume.
    ///
    /// The cache-resident half of the check point is an ordinary metadata
    /// flush; this is the remainder — blocks a rollback dropped or an eviction
    /// spilled, whose only copy is the log.
    pub fn checkpoint_logged(
        &mut self,
        device: &dyn BlockDevice,
        cursor: u32,
        budget: usize,
        limit: u32,
    ) -> Result<CheckpointProgress, Ext2Error> {
        let Some(mut journal) = self.journal.take() else {
            return Ok(CheckpointProgress {
                cursor: 0,
                more: false,
            });
        };
        let end = limit.min(journal.head());
        let mut slot = cursor.max(1);
        let mut written = 0usize;
        let mut result = Ok(());
        while slot < end {
            if written >= budget {
                break;
            }
            let block = journal.slot_block_at(slot);
            slot += 1;
            // Skipped only when the cache says the home matches, which a
            // clean entry does by construction. A dirty entry may be newer,
            // but nothing says the metadata phase has run for its epoch, so
            // the committed copy goes home regardless — the newest slot wins,
            // and a newer cache copy follows in a later pass.
            if block == 0 || self.home_matches(block) {
                continue;
            }
            if let Err(e) = journal.copy_to_home(slot - 1, block, device) {
                result = Err(e);
                break;
            }
            written += 1;
        }
        self.unbarriered += journal.take_writes();
        self.journal = Some(journal);
        result?;
        Ok(CheckpointProgress {
            cursor: slot,
            more: slot < end,
        })
    }

    /// Whether the cache says `block`'s home location already holds its
    /// newest contents.
    fn home_matches(&self, block: u32) -> bool {
        self.index
            .get(&BlockNum(block))
            .is_some_and(|&slot| self.entries[slot].valid && !self.entries[slot].frame.dirty())
    }

    /// Where the log's append point stands, or 1 when there is no log.
    pub fn journal_head(&self) -> u32 {
        self.journal.as_ref().map_or(1, |j| j.head())
    }

    /// Which emptying of the log the current slot indices belong to. A pass
    /// records it so one resumed after another reset the log cannot mistake
    /// its own indices for the new generation's.
    pub fn journal_generation(&self) -> u32 {
        self.journal.as_ref().map_or(0, |j| j.generation())
    }

    /// Drop every clean entry.
    ///
    /// A replay writes home locations directly, so anything the cache read
    /// before it ran may now be stale. Clean only, because a dirty block is
    /// newer than the medium by construction and a replay runs before any
    /// operation can have dirtied one.
    pub fn invalidate_all_clean(&mut self) {
        for i in 0..self.entries.len() {
            if self.entries[i].valid && !self.entries[i].frame.dirty() {
                self.drop_entry(i);
            }
        }
    }

    /// Declare the log check pointed. The caller must have barriered the home
    /// writes first.
    ///
    /// Its own write is deliberately not counted as owing a barrier: losing it
    /// costs a redundant replay of transactions already applied, which writes
    /// the same bytes to the same places, and counting it would leave a sync
    /// that had nothing else to do reporting itself perpetually unfinished.
    pub fn journal_reset(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        let Some(mut journal) = self.journal.take() else {
            return Ok(());
        };
        let result = journal.reset(device);
        let _ = journal.take_writes();
        self.journal = Some(journal);
        result
    }

    /// Whether the log holds transactions a mount would have to replay.
    pub fn journal_is_empty(&self) -> bool {
        self.journal.as_ref().is_none_or(|j| j.is_empty())
    }

    pub fn dirty_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.valid && e.frame.dirty())
            .count()
    }

    /// Invalidate a cached block (evict without writing).
    ///
    /// Inside an open operation this is *deferred* to the commit rather than
    /// applied at once. Dropping the slot immediately would throw away the
    /// undo snapshot that the block's own record names, so a later rollback
    /// would silently revert it to whatever the device last held instead of to
    /// its pre-operation contents. Deferring costs nothing: the callers
    /// invalidate blocks they are *freeing*, and a reallocation reaches them
    /// through `get_zero_*`, which overwrites regardless.
    pub fn invalidate(&mut self, block: BlockNum) {
        let Some(&slot) = self.index.get(&block) else {
            return;
        };
        if self.op_depth > 0 {
            self.entries[slot].op_invalidated = true;
            return;
        }
        self.drop_entry(slot);
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
            // Only a valid slot owns its index entry: `drop_entry` leaves
            // `block` set on the slot it invalidates, so removing on that
            // stale number would unindex whichever live slot holds it now.
            if self.entries[i].valid {
                self.index.remove(&self.entries[i].block);
            }
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

        // A block the open operation dirtied is the victim of last resort.
        // Its home must not receive uncommitted content, so with a log it
        // goes there instead, retracted by the abort rewind; without one it is
        // written home, the residual a log exists to close.
        let mut slot = None;
        for touched_ok in [false, true] {
            let mut best_lru = u64::MAX;
            for (i, entry) in self.entries.iter().enumerate() {
                if entry.pinned != 0 || (entry.op_touched && !touched_ok) {
                    continue;
                }
                if entry.lru < best_lru {
                    best_lru = entry.lru;
                    slot = Some(i);
                }
            }
            if slot.is_some() {
                break;
            }
        }

        let slot = slot.ok_or(Ext2Error::DeviceError)?;

        // Eviction is a cache-replacement event, not a durability point, so no
        // device barrier is issued here; the commit and FS-level `sync`
        // provide the ordering, which is why a data block written home from
        // here is recorded as owing one.
        if self.entries[slot].frame.dirty() {
            let bs = self.block_size as usize;
            let spill = self.journal.is_some()
                && self.entries[slot].op_touched
                && self.entries[slot].kind == BlockKind::Metadata;
            if spill {
                let staged = self.journal.take();
                let block = self.entries[slot].block.raw();
                let outcome = match staged {
                    Some(mut journal) => {
                        let r = journal.spill(
                            block,
                            &self.entries[slot].frame.as_bytes()[..bs],
                            device,
                        );
                        self.unbarriered += journal.take_writes();
                        self.journal = Some(journal);
                        r
                    }
                    None => Ok(()),
                };
                outcome?;
            } else {
                let offset = self.entries[slot].block.to_disk_offset(self.block_size);
                device
                    .write_at(offset.raw(), &self.entries[slot].frame.as_bytes()[..bs])
                    .map_err(|_| Ext2Error::DeviceError)?;
                self.unbarriered += 1;
                if self.entries[slot].op_touched && self.entries[slot].kind == BlockKind::Data {
                    self.op_data_evicted = true;
                }
            }
        }

        self.index.remove(&self.entries[slot].block);
        let entry = &mut self.entries[slot];
        entry.valid = false;
        entry.frame.set_dirty(false);
        entry.frame.set_owner_key(0);
        entry.op_touched = false;
        entry.op_invalidated = false;
        entry.op_discard = false;
        entry.op_restored = false;

        Ok(slot)
    }
}

/// What one [`BlockCache::flush_bounded`] step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushProgress {
    pub written: usize,
    /// Blocks in this pass's epoch are still dirty: the step ran out of
    /// budget, not of work.
    pub more: bool,
}

/// Where a check point reached, so the next step resumes there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointProgress {
    pub cursor: u32,
    pub more: bool,
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
        let epoch = self.cache.epoch;
        let entry = &mut self.cache.entries[self.slot];
        entry.frame.set_dirty(true);
        entry.dirty_epoch = epoch;
        &mut entry.frame.as_bytes_mut()[..bs]
    }

    /// A fixed-size window into the block, or `None` if it does not fit.
    ///
    /// Parsers take the array rather than a slice, so a short or misplaced
    /// window is a `None` at the caller instead of a length the parser trusts.
    pub fn window<const N: usize>(&self, at: usize) -> Option<&[u8; N]> {
        let data = self.data();
        let end = at.checked_add(N)?;
        if end > data.len() {
            return None;
        }
        data[at..end].try_into().ok()
    }

    pub fn window_mut<const N: usize>(&mut self, at: usize) -> Option<&mut [u8; N]> {
        let data = self.data_mut();
        let end = at.checked_add(N)?;
        if end > data.len() {
            return None;
        }
        (&mut data[at..end]).try_into().ok()
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
