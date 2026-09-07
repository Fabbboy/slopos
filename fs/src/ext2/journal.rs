//! A physical redo log for ext2 metadata.
//!
//! The log lives in the blocks of an ordinary preallocated file, so `e2fsck`
//! sees a file and this kernel needs no on-disk feature bit and no jbd2
//! format compatibility. Nothing outside those blocks is written until a
//! transaction's commit record is on the medium: that is what lets a failed
//! operation be retracted and an unclean image be repaired rather than
//! refused.
//!
//! # Log format
//!
//! Slot 0 is the log's own superblock. Slots 1.. are records:
//!
//! | record   | slots   | meaning                                          |
//! |----------|---------|--------------------------------------------------|
//! | `DATA`   | 1 + *n* | the *n* payload slots hold the listed blocks     |
//! | `REVOKE` | 1       | the listed blocks must not be replayed from here |
//! | `COMMIT` | 1       | the records before it, this sequence, are final  |
//!
//! Recovery scans from slot 1 expecting the sequence the superblock names and
//! stops at the first record that is not the next committed transaction. The
//! committed region is then applied **in slot order**, so the last write of a
//! block wins and a `REVOKE` cancels every earlier write of the blocks it
//! lists — which is what makes a block freed and reused as file data safe.
//!
//! A payload is usually metadata, but a small write's file data is logged the
//! same way instead of being written home and barriered behind: replay then
//! restores both or neither, for a block write rather than a device flush.
//!
//! The commit record's CRC covers every byte of the transaction's other
//! records, so a torn or lost log write makes the transaction fail its own
//! check. That is what removes the barrier before the commit record — the
//! trade ext4 makes with `async_commit`.

use slopos_ostd::{KBox, KVec};

use super::Ext2Error;
use crate::blockdev::BlockDevice;
use crate::verity::{CRC32_INIT, crc32_feed, crc32_finish};

/// "SLJS", the log superblock in slot 0.
const SB_MAGIC: u32 = 0x534A_4C53;
/// "SLJR", every record header.
const REC_MAGIC: u32 = 0x524A_4C53;
const FORMAT_VERSION: u32 = 1;

const REC_DATA: u32 = 1;
const REC_REVOKE: u32 = 2;
const REC_COMMIT: u32 = 3;

/// Fixed part of a record header; the block numbers follow.
const REC_ENTRIES_OFF: usize = 24;
/// Where the log superblock records what volume and file it belongs to.
const SB_IDENTITY_OFF: usize = 20;
/// Bytes of the log superblock the CRC covers; the CRC itself follows them.
const SB_CRC_SPAN: usize = 24;

/// Smallest log worth attaching. An operation whose metadata does not fit
/// refuses, and refusing a routine `create` would be worse than having no
/// journal at all.
pub const MIN_LOG_SLOTS: u32 = 32;

/// The volume the log belongs to, so a target block read off the medium can be
/// refused before it becomes a write offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogExtent {
    pub first_data_block: u32,
    pub blocks_count: u32,
}

/// What attaching a log did, for the mount log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalRecovery {
    /// Committed transactions the scan found and replayed.
    pub transactions: u32,
    /// Blocks written back to their home locations by the replay.
    pub blocks: u32,
}

impl JournalRecovery {
    pub const NONE: Self = Self {
        transactions: 0,
        blocks: 0,
    };

    pub fn replayed(self) -> bool {
        self.transactions > 0
    }
}

fn le32(data: &[u8], at: usize) -> u32 {
    let mut raw = [0u8; 4];
    raw.copy_from_slice(&data[at..at + 4]);
    u32::from_le_bytes(raw)
}

fn put_le32(data: &mut [u8], at: usize, value: u32) {
    data[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// One record header, as read off the medium.
#[derive(Debug, Clone, Copy)]
struct RecHeader {
    kind: u32,
    count: u32,
    crc: u32,
}

pub struct Journal {
    /// Home block of each log slot. `slots[0]` is the log superblock.
    slots: KVec<u32>,
    /// The filesystem block whose newest content is in this slot, or zero.
    /// Scanned backwards to look one up and forwards to check point, which is
    /// what makes "the last write wins" fall out of the array order.
    slot_block: KVec<u32>,
    /// Blocks freed by the open operation, awaiting a `REVOKE` record.
    revokes: KVec<u32>,
    /// Mappings a `REVOKE` cleared, so an abort can put them back.
    revoke_undo: KVec<(u32, u32)>,
    /// Staging buffer for record headers. Preallocated: a block does not fit
    /// the 2 KiB stack budget and a commit must not allocate.
    header: KVec<u8>,
    /// Second staging buffer, for reading a slot back during a check point.
    transfer: KVec<u8>,
    block_size: u32,
    inode: u32,
    /// Every block the log writes to is checked against this range: a record's
    /// target becomes a write offset and the record came off the medium.
    blocks_count: u32,
    first_data_block: u32,
    /// Next free slot; 1 exactly when the log is empty.
    head: u32,
    /// Sequence the open transaction writes. Never reset, so a stale record
    /// left beyond the committed region can never be mistaken for the next
    /// transaction.
    seq: u32,
    /// Bumped by every [`Self::reset`]. A writeback pass records it, so a
    /// pass resumed after another emptied and refilled the log cannot mistake
    /// its own slot indices for the new generation's.
    generation: u32,
    /// `head` when the open operation began, for the abort rewind.
    op_head: u32,
    /// Running CRC over the open transaction's records.
    crc: u32,
    /// Device writes issued since the caller last took the count. The cache
    /// owns the barrier accounting, so the log only reports.
    writes: usize,
}

impl Journal {
    /// Take over `slots` as a log: validate the superblock, replay whatever a
    /// previous boot committed and did not check point, and leave the log
    /// empty. `slots[0]` is spent on the log superblock.
    ///
    /// Boxed, and `#[inline(never)]`: a mount-path frame cannot carry the
    /// struct alongside the block list and the inode under the 2 KiB gate.
    #[inline(never)]
    pub fn attach(
        slots: KVec<u32>,
        block_size: u32,
        inode: u32,
        extent: LogExtent,
        device: &dyn BlockDevice,
    ) -> Result<(KBox<Self>, JournalRecovery), Ext2Error> {
        if slots.len() < MIN_LOG_SLOTS as usize + 1 {
            return Err(Ext2Error::NoSpace);
        }
        // Refused rather than clamped: a slot outside the volume means the
        // mapping is not this file's, and past the filesystem extent is where
        // a verity trailer keeps the hashes that would have detected it.
        for block in slots.as_slice() {
            if *block < extent.first_data_block || *block >= extent.blocks_count {
                return Err(Ext2Error::InvalidBlock);
            }
        }
        let count = slots.len();
        let mut journal = Self {
            slots,
            slot_block: KVec::zeroed(count).map_err(|_| Ext2Error::OutOfMemory)?,
            revokes: KVec::with_capacity(entries_per_header(block_size))
                .map_err(|_| Ext2Error::OutOfMemory)?,
            revoke_undo: KVec::with_capacity(count).map_err(|_| Ext2Error::OutOfMemory)?,
            header: KVec::zeroed(block_size as usize).map_err(|_| Ext2Error::OutOfMemory)?,
            transfer: KVec::zeroed(block_size as usize).map_err(|_| Ext2Error::OutOfMemory)?,
            block_size,
            inode,
            blocks_count: extent.blocks_count,
            first_data_block: extent.first_data_block,
            head: 1,
            seq: 1,
            generation: 0,
            op_head: 1,
            crc: CRC32_INIT,
            writes: 0,
        };
        // A superblock that does not describe this file on this volume is one
        // this boot must not read; the reset below overwrites it.
        let recovery = match journal.read_superblock(device)? {
            Some(seq) => {
                journal.seq = seq;
                journal.replay(device)?
            }
            None => JournalRecovery::NONE,
        };
        journal.reset(device)?;
        let journal = KBox::try_new(journal).map_err(|_| Ext2Error::OutOfMemory)?;
        Ok((journal, recovery))
    }

    /// Whether `block` is a block of this volume, and so a legal write target.
    fn in_volume(&self, block: u32) -> bool {
        block >= self.first_data_block && block < self.blocks_count
    }

    /// Which emptying of the log the current slot indices belong to.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn inode(&self) -> u32 {
        self.inode
    }

    /// Log slots, the superblock's excluded.
    pub fn capacity(&self) -> u32 {
        (self.slots.len() as u32).saturating_sub(1)
    }

    pub fn free_slots(&self) -> u32 {
        (self.slots.len() as u32).saturating_sub(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.head <= 1
    }

    /// The log has enough room left that an ordinary operation will fit
    /// without a check point first.
    pub fn has_headroom(&self) -> bool {
        self.free_slots() >= self.low_water()
    }

    /// The log is filling and the flusher should drain it, well before an
    /// operation is forced to check point inline.
    ///
    /// Twice the low-water mark, not a hair above it: a drain is a whole check
    /// point, so a threshold the next operation crosses again turns one pass
    /// per burst into one pass per operation.
    pub fn needs_drain(&self) -> bool {
        self.free_slots() < self.low_water().saturating_mul(2)
    }

    fn low_water(&self) -> u32 {
        (self.capacity() / 2).min(256).max(MIN_LOG_SLOTS)
    }

    /// Entries one record header can list.
    pub fn max_entries(&self) -> usize {
        entries_per_header(self.block_size)
    }

    pub fn take_writes(&mut self) -> usize {
        core::mem::take(&mut self.writes)
    }

    /// Where a block's newest content lives, when that is the log rather than
    /// the block's own home. Scans backwards so the newest record wins.
    pub fn resident_slot(&self, block: u32) -> Option<u32> {
        if block == 0 {
            return None;
        }
        let mut slot = self.head;
        while slot > 1 {
            slot -= 1;
            if self.slot_block[slot as usize] == block {
                return Some(slot);
            }
        }
        None
    }

    /// Copy a slot's payload into `out`, which must be one block long.
    pub fn read_slot(
        &self,
        slot: u32,
        device: &dyn BlockDevice,
        out: &mut [u8],
    ) -> Result<(), Ext2Error> {
        let offset = self.slot_offset(slot)?;
        device
            .read_at(offset, out)
            .map_err(|_| Ext2Error::DeviceError)
    }

    pub fn begin_op(&mut self) {
        self.op_head = self.head;
        self.revokes.clear();
        self.revoke_undo.clear();
        self.crc = CRC32_INIT;
    }

    /// Discard everything the open operation appended.
    ///
    /// Sound because no home block was written on its behalf, and the caller
    /// drops the operation's cache entries, so a later read comes back from
    /// the log or from the block's own home.
    pub fn abort_op(&mut self) {
        for slot in self.op_head..self.head {
            self.slot_block[slot as usize] = 0;
        }
        while let Some((slot, block)) = self.revoke_undo.pop() {
            self.slot_block[slot as usize] = block;
        }
        self.head = self.op_head;
        self.revokes.clear();
        self.crc = CRC32_INIT;
    }

    /// The open operation appended nothing, so there is no transaction to
    /// commit and the sequence is not spent.
    pub fn op_is_empty(&self) -> bool {
        self.head == self.op_head
    }

    /// Note that `block` was freed, so no record before this point may be
    /// replayed into it. Flushes the queue when it fills, which is what keeps
    /// a long truncate's revoke list bounded.
    pub fn note_revoke(&mut self, block: u32, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if block == 0 {
            return Ok(());
        }
        self.revokes
            .push(block)
            .map_err(|_| Ext2Error::OutOfMemory)?;
        if self.revokes.len() >= self.max_entries() {
            self.flush_revokes(device)?;
        }
        Ok(())
    }

    /// Emit the queued revokes. Called before any payload record, so a block
    /// freed and then reallocated as metadata within one operation is
    /// described by the later record rather than cancelled by the revoke.
    pub fn flush_revokes(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if self.revokes.is_empty() {
            return Ok(());
        }
        let slot = self.reserve(1)?;
        let count = self.revokes.len() as u32;
        self.header.as_mut_slice().fill(0);
        put_le32(self.header.as_mut_slice(), 0, REC_MAGIC);
        put_le32(self.header.as_mut_slice(), 4, self.seq);
        put_le32(self.header.as_mut_slice(), 8, REC_REVOKE);
        put_le32(self.header.as_mut_slice(), 12, count);
        for (i, block) in self.revokes.as_slice().iter().enumerate() {
            put_le32(self.header.as_mut_slice(), REC_ENTRIES_OFF + i * 4, *block);
        }
        self.write_slot_from_header(slot, device)?;

        // The in-memory half of the same rule. A mapping from *before* this
        // operation is recorded before it is cleared, because an abort makes
        // the block the inode's again and its committed content is still only
        // in the log; one this operation made needs no record, since the
        // rewind discards the record it names.
        for i in 0..self.revokes.len() {
            let block = self.revokes.as_slice()[i];
            for s in 1..slot {
                if self.slot_block[s as usize] != block {
                    continue;
                }
                if s < self.op_head {
                    self.revoke_undo
                        .push((s, block))
                        .map_err(|_| Ext2Error::OutOfMemory)?;
                }
                self.slot_block[s as usize] = 0;
            }
        }
        self.revokes.clear();
        Ok(())
    }

    /// Reserve a payload record for `targets` and return its first payload
    /// slot. The caller writes one payload per target, in order, with
    /// [`Self::write_payload`].
    pub fn begin_payloads(
        &mut self,
        targets: &[u32],
        device: &dyn BlockDevice,
    ) -> Result<u32, Ext2Error> {
        debug_assert!(targets.len() <= self.max_entries());
        self.flush_revokes(device)?;
        let header_slot = self.reserve(1 + targets.len() as u32)?;
        self.header.as_mut_slice().fill(0);
        put_le32(self.header.as_mut_slice(), 0, REC_MAGIC);
        put_le32(self.header.as_mut_slice(), 4, self.seq);
        put_le32(self.header.as_mut_slice(), 8, REC_DATA);
        put_le32(self.header.as_mut_slice(), 12, targets.len() as u32);
        for (i, block) in targets.iter().enumerate() {
            put_le32(self.header.as_mut_slice(), REC_ENTRIES_OFF + i * 4, *block);
        }
        self.write_slot_from_header(header_slot, device)?;
        let first = header_slot + 1;
        for (i, block) in targets.iter().enumerate() {
            self.slot_block[first as usize + i] = *block;
        }
        Ok(first)
    }

    pub fn write_payload(
        &mut self,
        slot: u32,
        data: &[u8],
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        let offset = self.slot_offset(slot)?;
        device
            .write_at(offset, data)
            .map_err(|_| Ext2Error::DeviceError)?;
        self.crc = crc32_feed(self.crc, data);
        self.writes += 1;
        Ok(())
    }

    /// Put one block into the log on its own, so the cache can give its slot
    /// away without the block's home ever holding uncommitted content.
    pub fn spill(
        &mut self,
        block: u32,
        data: &[u8],
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        let first = self.begin_payloads(&[block], device)?;
        self.write_payload(first, data, device)
    }

    /// Close the transaction. Its records become replayable the moment this
    /// block reaches the medium, and unreadable garbage if it does not.
    pub fn write_commit(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        self.flush_revokes(device)?;
        let slot = self.reserve(1)?;
        let crc = crc32_finish(self.crc);
        self.header.as_mut_slice().fill(0);
        put_le32(self.header.as_mut_slice(), 0, REC_MAGIC);
        put_le32(self.header.as_mut_slice(), 4, self.seq);
        put_le32(self.header.as_mut_slice(), 8, REC_COMMIT);
        put_le32(self.header.as_mut_slice(), 16, crc);
        // Outside the CRC it carries, so this write is not fed back in.
        let offset = self.slot_offset(slot)?;
        device
            .write_at(offset, &self.header.as_slice()[..self.block_size as usize])
            .map_err(|_| Ext2Error::DeviceError)?;
        self.writes += 1;
        self.seq = self.seq.wrapping_add(1);
        self.revoke_undo.clear();
        self.crc = CRC32_INIT;
        Ok(())
    }

    /// Copy a logged block to its home location. The log, never the cache, is
    /// the source: the cache may hold an operation's uncommitted changes to
    /// the same block.
    pub fn copy_to_home(
        &mut self,
        slot: u32,
        block: u32,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        if !self.in_volume(block) {
            return Err(Ext2Error::InvalidBlock);
        }
        let bs = self.block_size as usize;
        let from = self.slot_offset(slot)?;
        device
            .read_at(from, &mut self.transfer.as_mut_slice()[..bs])
            .map_err(|_| Ext2Error::DeviceError)?;
        device
            .write_at(
                block as u64 * self.block_size as u64,
                &self.transfer.as_slice()[..bs],
            )
            .map_err(|_| Ext2Error::DeviceError)?;
        self.writes += 1;
        Ok(())
    }

    pub fn head(&self) -> u32 {
        self.head
    }

    pub fn slot_block_at(&self, slot: u32) -> u32 {
        self.slot_block
            .as_slice()
            .get(slot as usize)
            .copied()
            .unwrap_or(0)
    }

    /// The log's own blocks, in slot order.
    pub fn slots(&self) -> &[u32] {
        self.slots.as_slice()
    }

    /// Revokes queued for the open operation, not yet in a record.
    pub fn queued_revokes(&self) -> usize {
        self.revokes.len()
    }

    /// Declare every logged block checked pointed: the log is empty again.
    ///
    /// The caller must have barriered the home-location writes first. Losing
    /// this superblock write costs a redundant replay of transactions already
    /// applied, which writes the same bytes to the same places.
    pub fn reset(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        self.header.as_mut_slice().fill(0);
        put_le32(self.header.as_mut_slice(), 0, SB_MAGIC);
        put_le32(self.header.as_mut_slice(), 4, FORMAT_VERSION);
        put_le32(self.header.as_mut_slice(), 8, self.block_size);
        let capacity = self.capacity();
        put_le32(self.header.as_mut_slice(), 12, capacity);
        put_le32(self.header.as_mut_slice(), 16, self.seq);
        let identity = self.identity();
        put_le32(self.header.as_mut_slice(), SB_IDENTITY_OFF, identity);
        let crc = crate::verity::crc32(&self.header.as_slice()[..SB_CRC_SPAN]);
        put_le32(self.header.as_mut_slice(), SB_CRC_SPAN, crc);
        let offset = self.slot_offset(0)?;
        device
            .write_at(offset, &self.header.as_slice()[..self.block_size as usize])
            .map_err(|_| Ext2Error::DeviceError)?;
        self.writes += 1;
        self.head = 1;
        self.op_head = 1;
        self.generation = self.generation.wrapping_add(1);
        self.slot_block.as_mut_slice().fill(0);
        self.revokes.clear();
        self.revoke_undo.clear();
        self.crc = CRC32_INIT;
        Ok(())
    }

    fn slot_offset(&self, slot: u32) -> Result<u64, Ext2Error> {
        let block = self
            .slots
            .as_slice()
            .get(slot as usize)
            .copied()
            .ok_or(Ext2Error::InvalidBlock)?;
        Ok(block as u64 * self.block_size as u64)
    }

    fn reserve(&mut self, want: u32) -> Result<u32, Ext2Error> {
        let slot = self.head;
        let end = slot.checked_add(want).ok_or(Ext2Error::NoSpace)?;
        if end as usize > self.slots.len() {
            return Err(Ext2Error::NoSpace);
        }
        self.head = end;
        Ok(slot)
    }

    fn write_slot_from_header(
        &mut self,
        slot: u32,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        let bs = self.block_size as usize;
        let offset = self.slot_offset(slot)?;
        device
            .write_at(offset, &self.header.as_slice()[..bs])
            .map_err(|_| Ext2Error::DeviceError)?;
        self.crc = crc32_feed(self.crc, &self.header.as_slice()[..bs]);
        self.writes += 1;
        Ok(())
    }

    /// The sequence a scan should expect at slot 1, or `None` for a log this
    /// boot must not read.
    ///
    /// Geometry alone is satisfied by a log built for a *different*
    /// filesystem of the same shape, and replaying that one writes its
    /// metadata into this volume — hence the identity field.
    fn read_superblock(&mut self, device: &dyn BlockDevice) -> Result<Option<u32>, Ext2Error> {
        let bs = self.block_size as usize;
        let offset = self.slot_offset(0)?;
        device
            .read_at(offset, &mut self.header.as_mut_slice()[..bs])
            .map_err(|_| Ext2Error::DeviceError)?;
        let identity = self.identity();
        let data = self.header.as_slice();
        if le32(data, 0) != SB_MAGIC || le32(data, 4) != FORMAT_VERSION {
            return Ok(None);
        }
        if le32(data, 8) != self.block_size || le32(data, 12) != self.capacity() {
            return Ok(None);
        }
        if le32(data, SB_IDENTITY_OFF) != identity {
            return Ok(None);
        }
        if crate::verity::crc32(&data[..SB_CRC_SPAN]) != le32(data, SB_CRC_SPAN) {
            return Ok(None);
        }
        Ok(Some(le32(data, 16)))
    }

    /// What ties this log to this file on this volume. Not a hash: a mismatch
    /// means "not mine", which is all the mount needs to refuse a replay.
    fn identity(&self) -> u32 {
        let first = self.slots.as_slice().get(1).copied().unwrap_or(0);
        self.inode.wrapping_mul(0x0100_0193) ^ first.wrapping_mul(0x0100_0193) ^ self.blocks_count
    }

    /// The record at `slot`, or `None` when it is not the `expect` sequence's
    /// — which is how a scan finds the end of the committed region without a
    /// terminator.
    fn read_header(
        &mut self,
        slot: u32,
        expect: u32,
        device: &dyn BlockDevice,
    ) -> Result<Option<RecHeader>, Ext2Error> {
        if slot as usize >= self.slots.len() {
            return Ok(None);
        }
        let bs = self.block_size as usize;
        let offset = self.slot_offset(slot)?;
        device
            .read_at(offset, &mut self.header.as_mut_slice()[..bs])
            .map_err(|_| Ext2Error::DeviceError)?;
        let data = self.header.as_slice();
        if le32(data, 0) != REC_MAGIC || le32(data, 4) != expect {
            return Ok(None);
        }
        Ok(Some(RecHeader {
            kind: le32(data, 8),
            count: le32(data, 12),
            crc: le32(data, 16),
        }))
    }

    /// Apply every committed transaction the log holds.
    ///
    /// Two passes over the same records: the first decides how far the
    /// committed region reaches, because a transaction is only replayable once
    /// its own commit record checks out; the second builds the disposition of
    /// each slot and writes the survivors home.
    fn replay(&mut self, device: &dyn BlockDevice) -> Result<JournalRecovery, Ext2Error> {
        let first_seq = self.seq;
        let mut expect = first_seq;
        let mut end = 1u32;
        let mut transactions = 0u32;
        while let Some(next) = self.scan_transaction(end, expect, device)? {
            end = next;
            expect = expect.wrapping_add(1);
            transactions += 1;
        }
        if transactions == 0 {
            return Ok(JournalRecovery::NONE);
        }
        self.build_disposition(end, first_seq, device)?;
        let blocks = self.write_home(end, device)?;
        device.flush().map_err(|_| Ext2Error::DeviceError)?;
        self.seq = expect;
        self.head = end;
        Ok(JournalRecovery {
            transactions,
            blocks,
        })
    }

    /// Where the transaction beginning at `from` ends, if it committed and its
    /// CRC agrees with what is on the medium.
    fn scan_transaction(
        &mut self,
        from: u32,
        expect: u32,
        device: &dyn BlockDevice,
    ) -> Result<Option<u32>, Ext2Error> {
        let bs = self.block_size as usize;
        let mut slot = from;
        let mut crc = CRC32_INIT;
        loop {
            let Some(header) = self.read_header(slot, expect, device)? else {
                return Ok(None);
            };
            match header.kind {
                REC_COMMIT => {
                    return if crc32_finish(crc) == header.crc {
                        Ok(Some(slot + 1))
                    } else {
                        Ok(None)
                    };
                }
                REC_REVOKE => {
                    if header.count as usize > self.max_entries() {
                        return Ok(None);
                    }
                    crc = crc32_feed(crc, &self.header.as_slice()[..bs]);
                    slot += 1;
                }
                REC_DATA => {
                    let count = header.count;
                    if count as usize > self.max_entries()
                        || slot as usize + 1 + count as usize > self.slots.len()
                    {
                        return Ok(None);
                    }
                    // A target becomes a write offset and it came off the
                    // medium, so one outside the volume ends the committed
                    // region — the disposition every malformed field gets.
                    for i in 0..count as usize {
                        let block = le32(self.header.as_slice(), REC_ENTRIES_OFF + i * 4);
                        if !self.in_volume(block) {
                            return Ok(None);
                        }
                    }
                    crc = crc32_feed(crc, &self.header.as_slice()[..bs]);
                    for i in 0..count {
                        let offset = self.slot_offset(slot + 1 + i)?;
                        device
                            .read_at(offset, &mut self.transfer.as_mut_slice()[..bs])
                            .map_err(|_| Ext2Error::DeviceError)?;
                        crc = crc32_feed(crc, &self.transfer.as_slice()[..bs]);
                    }
                    slot += 1 + count;
                }
                _ => return Ok(None),
            }
        }
    }

    /// Fill `slot_block` for the committed region below `end`, applying each
    /// `REVOKE` to the records before it.
    fn build_disposition(
        &mut self,
        end: u32,
        first_seq: u32,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        self.slot_block.as_mut_slice().fill(0);
        let mut expect = first_seq;
        let mut slot = 1u32;
        while slot < end {
            // Re-read rather than held from the scan: a second pass costs
            // reads the recovery path can afford, where a per-record
            // allocation is not.
            let Some(header) = self.read_header(slot, expect, device)? else {
                break;
            };
            match header.kind {
                REC_COMMIT => {
                    expect = expect.wrapping_add(1);
                    slot += 1;
                }
                REC_REVOKE => {
                    let count = (header.count as usize).min(self.max_entries());
                    for i in 0..count {
                        let block = le32(self.header.as_slice(), REC_ENTRIES_OFF + i * 4);
                        for s in 1..slot {
                            if self.slot_block[s as usize] == block {
                                self.slot_block[s as usize] = 0;
                            }
                        }
                    }
                    slot += 1;
                }
                REC_DATA => {
                    // Bounded here as well as in the scan: a device that
                    // answers differently on this second read must not be able
                    // to index out of the array.
                    let count = (header.count as usize).min(self.max_entries());
                    for i in 0..count {
                        let target = slot as usize + 1 + i;
                        if target >= self.slot_block.len() {
                            break;
                        }
                        let block = le32(self.header.as_slice(), REC_ENTRIES_OFF + i * 4);
                        if self.in_volume(block) {
                            self.slot_block[target] = block;
                        }
                    }
                    slot += 1 + header.count;
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn write_home(&mut self, end: u32, device: &dyn BlockDevice) -> Result<u32, Ext2Error> {
        let mut written = 0u32;
        for slot in 1..end.min(self.slot_block.len() as u32) {
            let block = self.slot_block[slot as usize];
            if block == 0 || !self.in_volume(block) {
                continue;
            }
            self.copy_to_home(slot, block, device)?;
            written += 1;
        }
        Ok(written)
    }
}

fn entries_per_header(block_size: u32) -> usize {
    (block_size as usize).saturating_sub(REC_ENTRIES_OFF) / 4
}
