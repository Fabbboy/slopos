//! PTY pair allocation, lifecycle, and data routing.
//!
//! # Pair-Level Atomicity (Phase 20)
//!
//! PTY master/slave pairs are created and destroyed as atomic units.
//! `PTY_ALLOC_LOCK` serialises all pair-level lifecycle transitions:
//!
//! - **`pty_alloc()`** — find two free slots + initialise both
//! - **`free_pair_if_unused()`** — check both unused + free both
//! - **`pty_open_slave()`** — validate slot is still a PTY slave + increment
//!   open count (prevents opening a mid-free slave)
//!
//! `PTY_ALLOC_LOCK` is **not** held during data-path operations (`read`,
//! `write`, `push_input`).  Per-TTY slot locks in `TTY_SLOTS` remain the
//! fast-path protection for those.
//!
//! # Lock Ordering
//!
//! `PTY_ALLOC_LOCK` → `TTY_SLOTS[i]` (never the reverse).

use slopos_lib::IrqMutex;

use super::driver::TtyDriverKind;
use super::table::{TTY_INPUT_WAITERS, TTY_SLOTS, find_free_slot, find_free_slot_excluding};
use super::{MAX_TTYS, Tty, TtyError, TtyIndex};

// ---------------------------------------------------------------------------
// Pair-level lifecycle lock
// ---------------------------------------------------------------------------

/// Serialisation lock for PTY pair lifecycle operations.
///
/// Protects the pair-level invariant: master and slave slots are either
/// *both* initialised or *both* free.  Acquired during pair creation,
/// pair destruction, and validated slave opens.
///
/// **Not** held during data-path operations (read, write, push_input).
static PTY_ALLOC_LOCK: IrqMutex<()> = IrqMutex::new(());

// ---------------------------------------------------------------------------
// Pair allocation
// ---------------------------------------------------------------------------

/// Allocate a PTY master/slave pair atomically.
///
/// Holds `PTY_ALLOC_LOCK` for the entire find-and-initialise sequence,
/// guaranteeing that no concurrent `pty_alloc()` or `free_pair_if_unused()`
/// can observe a half-initialised pair.
///
/// Returns the master `TtyIndex`; the slave index is embedded in the
/// master's `TtyDriverKind::PtyMaster { slave_idx }`.
pub fn pty_alloc() -> Result<TtyIndex, TtyError> {
    let _alloc = PTY_ALLOC_LOCK.lock();

    let master_slot = find_free_slot().ok_or(TtyError::NotAllocated)?;
    let slave_slot = find_free_slot_excluding(master_slot).ok_or(TtyError::NotAllocated)?;

    let master_idx = TtyIndex(master_slot as u8);
    let slave_idx = TtyIndex(slave_slot as u8);

    {
        let mut guard = TTY_SLOTS[master_slot].lock();
        *guard = Some(Tty::new_pty_master(master_idx, slave_idx));
    }
    {
        let mut guard = TTY_SLOTS[slave_slot].lock();
        *guard = Some(Tty::new_pty_slave(slave_idx, master_idx));
    }

    Ok(master_idx)
}

// ---------------------------------------------------------------------------
// Validated slave open (Phase 20)
// ---------------------------------------------------------------------------

/// Atomically validate that `idx` is still a live PTY slave and increment
/// its open count.
///
/// Acquires `PTY_ALLOC_LOCK` to prevent races with `free_pair_if_unused()`:
/// if a concurrent close is freeing the pair, this call will either see the
/// slot as `None` (and return `NotAllocated`) or succeed and increment the
/// open count (preventing the pair from being freed).
///
/// Also clears `hung_up` and `peer_closed` on the slave (re-open semantics),
/// and clears `peer_closed` on the paired master — matching the existing
/// `open_ref()` behaviour for PTY slaves.
pub fn pty_open_slave(idx: TtyIndex) -> Result<u32, TtyError> {
    let _alloc = PTY_ALLOC_LOCK.lock();

    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let (count, peer_idx) = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = guard.as_mut().ok_or(TtyError::NotAllocated)?;

        match tty.driver {
            TtyDriverKind::PtySlave { master_idx } => {
                tty.open_count = tty
                    .open_count
                    .checked_add(1)
                    .unwrap_or_else(|| panic!("pty slave open_count overflow for idx {}", idx.0));
                tty.hung_up = false;
                tty.peer_closed = false;
                (tty.open_count, master_idx)
            }
            _ => return Err(TtyError::NotAllocated),
        }
    };

    // Clear peer_closed on the master (same as open_ref does for PtySlave).
    clear_peer_closed(peer_idx);

    Ok(count)
}

// ---------------------------------------------------------------------------
// Data routing
// ---------------------------------------------------------------------------

pub fn master_write(slave_idx: TtyIndex, data: &[u8]) {
    for &byte in data {
        super::push_input(slave_idx, byte);
    }
}

pub fn slave_write(master_idx: TtyIndex, data: &[u8]) {
    let slot = master_idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let should_wake = {
        let mut guard = TTY_SLOTS[slot].lock();
        let Some(master) = guard.as_mut() else {
            return;
        };

        if master.peer_closed || master.hung_up {
            return;
        }

        for &byte in data {
            let _ = master.ldisc.input_char(byte);
        }

        master.ldisc.has_data()
    };

    if should_wake {
        TTY_INPUT_WAITERS[slot].wake_all();
    }
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

pub fn is_pty_slave(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }

    let guard = TTY_SLOTS[slot].lock();
    matches!(
        guard.as_ref().map(|tty| &tty.driver),
        Some(TtyDriverKind::PtySlave { .. })
    )
}

pub fn get_pty_number(idx: TtyIndex) -> Result<u32, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let guard = TTY_SLOTS[slot].lock();
    let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
    match tty.driver {
        TtyDriverKind::PtyMaster { slave_idx } => Ok(slave_idx.0 as u32),
        _ => Err(TtyError::NotAllocated),
    }
}

// ---------------------------------------------------------------------------
// Peer state management
// ---------------------------------------------------------------------------

pub fn mark_peer_closed(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.peer_closed = true;
    }
    drop(guard);
    TTY_INPUT_WAITERS[slot].wake_all();
}

pub(crate) fn clear_peer_closed(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.peer_closed = false;
    }
}

// ---------------------------------------------------------------------------
// Pair lifecycle (Phase 20: atomic check-and-free)
// ---------------------------------------------------------------------------

/// Free both sides of a PTY pair if both have `open_count == 0`.
///
/// Holds `PTY_ALLOC_LOCK` for the entire check-and-free sequence so that
/// no concurrent `pty_alloc()` or `pty_open_slave()` can observe a
/// partially freed pair (TOCTOU prevention).
pub fn free_pair_if_unused(idx: TtyIndex, peer_idx: TtyIndex) {
    let _alloc = PTY_ALLOC_LOCK.lock();

    let idx_slot = idx.0 as usize;
    let peer_slot = peer_idx.0 as usize;
    if idx_slot >= MAX_TTYS || peer_slot >= MAX_TTYS {
        return;
    }

    let idx_unused = {
        let guard = TTY_SLOTS[idx_slot].lock();
        matches!(guard.as_ref(), Some(tty) if tty.open_count == 0)
    };
    let peer_unused = {
        let guard = TTY_SLOTS[peer_slot].lock();
        matches!(guard.as_ref(), Some(tty) if tty.open_count == 0)
    };

    if !(idx_unused && peer_unused) {
        return;
    }

    {
        let mut guard = TTY_SLOTS[idx_slot].lock();
        *guard = None;
    }
    {
        let mut guard = TTY_SLOTS[peer_slot].lock();
        *guard = None;
    }
}
