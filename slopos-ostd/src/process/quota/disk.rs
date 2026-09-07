//! The per-principal disk-block ledger.
//!
//! ext2 records no owner for a block, so there is no on-disk object to hold a
//! linear token: the charge lives in a `.bss` slot keyed by the account it was
//! made against, grown when the allocator hands a block out and shrunk when
//! one is given back. [`account_release`](super::arena::account_release)
//! releases whatever is left.
//!
//! Read the ceiling for what it is: a bound on a principal's **outstanding**
//! allocations, not a persistent disk quota. A block outlives the process that
//! allocated it and the medium does not say whose it was, so what the ceiling
//! stops is one live process taking every block above ext2's own reserve.

use slopos_abi::quota::DiskBlocksAxis;

use super::arena::{TryChargeError, try_charge};
use super::token::ChargeSlot;
use crate::process::account::{AccountId, MAX_ACCOUNTS};

/// One row per account slot, so a charge is an index rather than a scan.
static ROWS: [ChargeSlot<DiskBlocksAxis>; MAX_ACCOUNTS] =
    [const { ChargeSlot::empty() }; MAX_ACCOUNTS];

/// The row `account` owns, or `None` for an out-of-range slot or one a
/// *different* account holds — the compare is what keeps a stale id from
/// touching a recycled slot's ledger.
fn row_for(account: AccountId) -> Option<&'static ChargeSlot<DiskBlocksAxis>> {
    let row = ROWS.get(account.slot() as usize)?;
    if row.is_occupied() && row.account() != account {
        return None;
    }
    Some(row)
}

/// Debit `blocks` from `account`. Takes no lock and allocates nothing, so it
/// is legal from inside a filesystem allocator holding that mount's lock.
pub fn charge_blocks(account: AccountId, blocks: u32) -> Result<(), TryChargeError> {
    if blocks == 0 {
        return Ok(());
    }
    let reservation = try_charge::<DiskBlocksAxis>(account, blocks)?;
    match row_for(account) {
        // No row for an out-of-range slot: dropping the reservation gives the
        // debit straight back, so the charge is vacuous rather than leaked.
        None => drop(reservation),
        Some(row) => row.grow(reservation),
    }
    Ok(())
}

/// Give back up to `blocks`, clamped at what the account holds: the process
/// freeing a block is not necessarily the one that allocated it.
pub fn refund_blocks(account: AccountId, blocks: u32) {
    if blocks == 0 {
        return;
    }
    if let Some(row) = row_for(account) {
        row.shrink(blocks);
    }
}

/// What `account` currently holds.
pub fn blocks_held(account: AccountId) -> u32 {
    row_for(account).map_or(0, |row| row.amount())
}

/// Release the whole row, at the point the account goes dark.
pub(super) fn release(account: AccountId) {
    if let Some(row) = row_for(account) {
        row.take();
    }
}
