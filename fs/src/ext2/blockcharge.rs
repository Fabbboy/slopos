//! Which principal a charged block belongs to.
//!
//! A `DiskBlocks` refund must credit the account that was *charged*, and the
//! caller of the free is routinely not it: a foreign `unlink`, the orphan
//! drain on the flusher, a mount-time recovery. ext2 records no owner on the
//! medium, and an `AccountId` must never be written to one — it is a live slot
//! index whose generations restart every boot — so the record lives here, per
//! mount, keyed on `(inode, account)` because two principals can both have
//! grown one file.
//!
//! It fails in one direction only: a block with no row is refunded to nobody,
//! never to a principal that did not pay. Eviction from a full table walks the
//! rows in turn, so it costs occupants in proportion instead of costing the
//! next allocator its whole attribution.
//!
//! Refunds settle at the commit: a rollback restores the bitmap, so the
//! operation still owes the blocks.

use slopos_abi::quota::ResourceKind;
use slopos_ostd::KVec;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota;

use super::Ext2Error;

/// `(inode, principal)` pairs with live charged blocks, per mount.
pub(crate) const MAX_ROWS: usize = 128;

/// Pairs one operation may touch: a create, the widest case, grows the new
/// inode and its parent.
const MAX_PENDING: usize = 16;

/// Committed attribution. `blocks == 0` marks a free slot.
struct Row {
    ino: u32,
    account: AccountId,
    blocks: u32,
}

/// One operation's uncommitted delta for a pair. An unoccupied slot names no
/// account.
struct Pending {
    ino: u32,
    account: AccountId,
    /// Blocks added to the row, net of the ones taken away.
    net: i64,
    /// Blocks to refund at the commit.
    refund: u32,
}

pub(crate) struct BlockCharges {
    rows: KVec<Row>,
    pending: KVec<Pending>,
    /// The row a free last matched: blocks of one inode are freed in a run, so
    /// this keeps a large truncate from re-scanning per block.
    hint: usize,
    /// Where the next eviction starts. A cursor rather than a search for the
    /// most crowded principal: one step instead of a scan, and it lands on
    /// each principal in proportion to its occupancy anyway.
    evict_next: usize,
}

impl BlockCharges {
    pub(crate) fn new() -> Result<Self, Ext2Error> {
        let mut rows = KVec::with_capacity(MAX_ROWS).map_err(|_| Ext2Error::OutOfMemory)?;
        for _ in 0..MAX_ROWS {
            rows.push(Row {
                ino: 0,
                account: AccountId::NONE,
                blocks: 0,
            })
            .map_err(|_| Ext2Error::OutOfMemory)?;
        }
        let mut pending = KVec::with_capacity(MAX_PENDING).map_err(|_| Ext2Error::OutOfMemory)?;
        for _ in 0..MAX_PENDING {
            pending
                .push(Pending {
                    ino: 0,
                    account: AccountId::NONE,
                    net: 0,
                    refund: 0,
                })
                .map_err(|_| Ext2Error::OutOfMemory)?;
        }
        Ok(Self {
            rows,
            pending,
            hint: 0,
            evict_next: 0,
        })
    }

    /// Note `blocks` charged to `account` for `ino`. An unattributable block
    /// — no inode, no principal, no room — stays charged; it loses only the
    /// refund.
    pub(crate) fn charge(&mut self, ino: Option<u32>, account: AccountId, blocks: u32) {
        if blocks == 0 || account.is_none() {
            return;
        }
        let Some(ino) = ino else {
            return;
        };
        if let Some(entry) = self.pending_slot(ino, account) {
            entry.net = entry.net.saturating_add(i64::from(blocks));
        }
    }

    /// Undo the record of a charge whose allocation never happened.
    ///
    /// Not [`Self::free`]: that reaches into a committed row when it finds
    /// nothing of this operation's, which for a block never allocated would
    /// credit a principal that paid nothing.
    pub(crate) fn cancel(&mut self, ino: Option<u32>, account: AccountId, blocks: u32) {
        if blocks == 0 || account.is_none() {
            return;
        }
        let Some(ino) = ino else {
            return;
        };
        let found = self
            .pending
            .iter_mut()
            .find(|entry| !entry.account.is_none() && entry.ino == ino && entry.account == account);
        if let Some(entry) = found {
            entry.net -= i64::from(blocks);
        }
    }

    /// Take `blocks` of `ino` off whoever is charged for them.
    ///
    /// Which of an inode's co-owners a partial free debits is arbitrary — no
    /// owner is recorded per block — but each take is clamped to its row, so
    /// none is credited past what it paid for that inode.
    pub(crate) fn free(&mut self, ino: Option<u32>, blocks: u32) {
        let Some(ino) = ino else {
            return;
        };
        let mut remaining = blocks;
        // This operation's own new blocks first, so freeing what it just
        // allocated cancels rather than reaching into an older owner's row.
        for entry in self.pending.iter_mut() {
            if remaining == 0 {
                break;
            }
            if entry.account.is_none() || entry.ino != ino || entry.net <= 0 {
                continue;
            }
            let take = remaining.min(clamp_u32(entry.net));
            entry.net -= i64::from(take);
            entry.refund = entry.refund.saturating_add(take);
            remaining -= take;
        }
        while remaining != 0 {
            let Some(idx) = self.row_with_room(ino) else {
                return;
            };
            let (account, held) = (self.rows[idx].account, self.rows[idx].blocks);
            let taken = self.pending_taken(ino, account);
            let available = held.saturating_sub(taken);
            let take = remaining.min(available);
            let Some(entry) = self.pending_slot(ino, account) else {
                return;
            };
            entry.net -= i64::from(take);
            entry.refund = entry.refund.saturating_add(take);
            remaining -= take;
            self.hint = idx;
        }
    }

    /// Publish the operation's deltas and pay out its refunds.
    pub(crate) fn commit(&mut self) {
        for idx in 0..self.pending.len() {
            if self.pending[idx].account.is_none() {
                continue;
            }
            let entry = &mut self.pending[idx];
            let (ino, account, net, refund) = (entry.ino, entry.account, entry.net, entry.refund);
            *entry = Pending {
                ino: 0,
                account: AccountId::NONE,
                net: 0,
                refund: 0,
            };
            self.apply(ino, account, net);
            quota::refund_blocks(account, refund);
        }
    }

    /// Drop the operation's deltas: the bitmap is back the way it was, so the
    /// rows already describe the truth.
    pub(crate) fn rollback(&mut self) {
        for entry in self.pending.iter_mut() {
            *entry = Pending {
                ino: 0,
                account: AccountId::NONE,
                net: 0,
                refund: 0,
            };
        }
    }

    fn apply(&mut self, ino: u32, account: AccountId, net: i64) {
        if net == 0 {
            return;
        }
        if let Some(idx) = self.row_index(ino, account) {
            let blocks = i64::from(self.rows[idx].blocks) + net;
            self.rows[idx].blocks = clamp_u32(blocks);
            return;
        }
        if net < 0 {
            return;
        }
        let Some(idx) = self.free_row() else {
            return;
        };
        self.rows[idx] = Row {
            ino,
            account,
            blocks: clamp_u32(net),
        };
    }

    fn row_index(&self, ino: u32, account: AccountId) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.blocks != 0 && row.ino == ino && row.account == account)
    }

    /// A row for `ino` this operation has not already emptied, hint first.
    fn row_with_room(&self, ino: u32) -> Option<usize> {
        let hinted = self.rows.get(self.hint).filter(|row| {
            row.blocks != 0 && row.ino == ino && row.blocks > self.pending_taken(ino, row.account)
        });
        if hinted.is_some() {
            return Some(self.hint);
        }
        self.rows.iter().position(|row| {
            row.blocks != 0 && row.ino == ino && row.blocks > self.pending_taken(ino, row.account)
        })
    }

    /// What this operation has already taken off `(ino, account)`'s row.
    fn pending_taken(&self, ino: u32, account: AccountId) -> u32 {
        self.pending
            .iter()
            .find(|entry| !entry.account.is_none() && entry.ino == ino && entry.account == account)
            .map(|entry| clamp_u32(-entry.net))
            .unwrap_or(0)
    }

    /// The pending slot for `(ino, account)`, claiming a free one if the pair
    /// has none. `None` once the operation has outgrown the table.
    fn pending_slot(&mut self, ino: u32, account: AccountId) -> Option<&mut Pending> {
        let existing = self.pending.iter().position(|entry| {
            !entry.account.is_none() && entry.ino == ino && entry.account == account
        });
        let idx = match existing {
            Some(idx) => idx,
            None => {
                let free = self
                    .pending
                    .iter()
                    .position(|entry| entry.account.is_none())?;
                self.pending[free].ino = ino;
                self.pending[free].account = account;
                free
            }
        };
        self.pending.get_mut(idx)
    }

    /// A row to write: an empty one, else one whose principal is gone and can
    /// no longer be refunded, else the next in eviction order.
    fn free_row(&mut self) -> Option<usize> {
        if let Some(idx) = self.rows.iter().position(|row| row.blocks == 0) {
            return Some(idx);
        }
        if let Some(idx) = self
            .rows
            .iter()
            .position(|row| quota::stats(row.account, ResourceKind::DiskBlocks).is_none())
        {
            return Some(idx);
        }
        self.evictable_row()
    }

    /// The next row in cursor order. Evicting it makes that principal's blocks
    /// unrefundable until it exits, and credits nobody.
    fn evictable_row(&mut self) -> Option<usize> {
        let len = self.rows.len();
        for step in 0..len {
            let idx = (self.evict_next + step) % len;
            if self.rows[idx].blocks == 0 {
                continue;
            }
            self.evict_next = (idx + 1) % len;
            return Some(idx);
        }
        None
    }
}

/// Saturating both ways: a negative net means the row was emptied.
fn clamp_u32(value: i64) -> u32 {
    value.clamp(0, i64::from(u32::MAX)) as u32
}
