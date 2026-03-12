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
//! # Generation-Safe Peer Identity (Phase 26)
//!
//! PTY peer references are wrapped in [`PtyPeerHandle`], which combines a
//! `TtyIndex` with a generation counter.  The generation counter
//! (`TTY_GENERATIONS[slot]`) is incremented each time a slot is freed.
//! Before any cross-end write, the stored generation is compared against
//! the current generation — a mismatch means the slot was freed and
//! potentially reused by an unrelated PTY pair, so the write is silently
//! discarded.
//!
//! # Lock Ordering
//!
//! `PTY_ALLOC_LOCK` → `TTY_SLOTS[i]` (never the reverse).

use core::sync::atomic::Ordering;

use slopos_lib::IrqMutex;

use super::driver::TtyDriverKind;
use super::table::{
    TTY_GENERATIONS, TTY_INPUT_WAITERS, TTY_SLOTS, find_free_slot, find_free_slot_excluding,
};
use super::{MAX_TTYS, Tty, TtyError, TtyIndex};

// ---------------------------------------------------------------------------
// Generation-safe peer identity (Phase 26)
// ---------------------------------------------------------------------------

/// A generation-tagged handle to a PTY peer slot.
///
/// Combines a `TtyIndex` (which slot) with a generation counter (which
/// incarnation of that slot).  Used inside `TtyDriverKind::PtyMaster` and
/// `TtyDriverKind::PtySlave` to prevent stale-slot misrouting after rapid
/// free/reuse cycles.
///
/// Before any cross-end data write, [`validate_peer`] compares the stored
/// generation against the current `TTY_GENERATIONS[slot]` — a mismatch
/// means the slot was freed and potentially reallocated to an unrelated
/// pair, so the write is silently discarded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PtyPeerHandle {
    /// The slot index of the peer TTY.
    pub idx: TtyIndex,
    /// The generation counter of the peer slot at the time the PTY pair
    /// was allocated.
    pub generation: u32,
}

impl PtyPeerHandle {
    /// Create a new peer handle capturing the current generation of `idx`.
    pub fn new(idx: TtyIndex, generation: u32) -> Self {
        Self { idx, generation }
    }

    /// Snapshot the current generation of `idx` into a new handle.
    pub fn snapshot(idx: TtyIndex) -> Self {
        let slot = idx.0 as usize;
        let generation = if slot < MAX_TTYS {
            TTY_GENERATIONS[slot].load(Ordering::Acquire)
        } else {
            0
        };
        Self { idx, generation }
    }
}

/// Returns `true` if `handle` still refers to the same incarnation of
/// its slot — i.e. the slot has not been freed and potentially reused
/// since the handle was created.
pub fn validate_peer(handle: &PtyPeerHandle) -> bool {
    let slot = handle.idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    TTY_GENERATIONS[slot].load(Ordering::Acquire) == handle.generation
}

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
/// Phase 26: peer cross-references now carry generation-tagged
/// [`PtyPeerHandle`]s so that stale writes after rapid free/reuse are
/// safely discarded.
///
/// Returns the master `TtyIndex`; the slave index is embedded in the
/// master's `TtyDriverKind::PtyMaster { peer }`.
pub fn pty_alloc() -> Result<TtyIndex, TtyError> {
    let _alloc = PTY_ALLOC_LOCK.lock();

    let master_slot = find_free_slot().ok_or(TtyError::NotAllocated)?;
    let slave_slot = find_free_slot_excluding(master_slot).ok_or(TtyError::NotAllocated)?;

    let master_idx = TtyIndex(master_slot as u8);
    let slave_idx = TtyIndex(slave_slot as u8);

    // Snapshot the current generation of each slot *before* initialisation.
    // These generations were set when the slots were last freed (or are 0
    // for never-used slots).
    let slave_peer = PtyPeerHandle::snapshot(slave_idx);
    let master_peer = PtyPeerHandle::snapshot(master_idx);

    {
        let mut guard = TTY_SLOTS[master_slot].lock();
        *guard = Some(Tty::new_pty_master(master_idx, slave_peer));
    }
    {
        let mut guard = TTY_SLOTS[slave_slot].lock();
        *guard = Some(Tty::new_pty_slave(slave_idx, master_peer));
    }

    Ok(master_idx)
}

// ---------------------------------------------------------------------------
// Validated slave open (Phase 20, Phase 38: lock guard)
// ---------------------------------------------------------------------------

/// Atomically validate that `idx` is still a live PTY slave, check the
/// slave lock, and increment its open count.
///
/// Acquires `PTY_ALLOC_LOCK` to prevent races with `free_pair_if_unused()`:
/// if a concurrent close is freeing the pair, this call will either see the
/// slot as `None` (and return `NotAllocated`) or succeed and increment the
/// open count (preventing the pair from being freed).
///
/// Phase 38: Returns `TtyError::PermissionDenied` if the slave is locked.
/// The master holder must call `TIOCSPTLCK` with arg=0 to unlock before
/// the slave can be opened.
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
            TtyDriverKind::PtySlave { ref peer } => {
                // Phase 38: Locked slaves cannot be opened.
                if tty.slave_locked {
                    return Err(TtyError::PermissionDenied);
                }
                tty.open_count = tty
                    .open_count
                    .checked_add(1)
                    .unwrap_or_else(|| panic!("pty slave open_count overflow for idx {}", idx.0));
                tty.hung_up = false;
                tty.peer_closed = false;
                (tty.open_count, peer.idx)
            }
            _ => return Err(TtyError::NotAllocated),
        }
    };

    // Clear peer_closed on the master (same as open_ref does for PtySlave).
    clear_peer_closed(peer_idx);

    Ok(count)
}

// ---------------------------------------------------------------------------
// Data routing (Phase 26: generation-validated)
// ---------------------------------------------------------------------------

/// Master write → push bytes into the slave's line discipline input.
///
/// Phase 26: validates the peer handle's generation before writing.
/// If the peer slot was freed and potentially reused, the write is
/// silently discarded.
///
/// Finishing Phase 2: Returns the number of bytes successfully pushed.
/// Stops early when the slave's cooked buffer hits the throttle
/// high-water mark (`throttled == true`), enabling short writes and
/// back-pressure.  The caller is responsible for retrying the remainder.
///
/// # Throttle granularity (design decision)
///
/// Throttle is checked once per `BATCH_SIZE` (64 bytes) rather than
/// per byte.  This is an intentional trade-off:
///
/// - **Per-byte checking** requires acquiring the per-slot `IrqMutex`
///   on every byte, turning an O(1) cost into O(n) lock/unlock cycles.
///   Linux avoids this in `n_tty_receive_buf_common` only because its
///   `TTY_THROTTLED` flag lives outside the line discipline lock.
///
/// - **Batch checking** allows up to `BATCH_SIZE - 1` bytes (63) to be
///   pushed past `THROTTLE_HIGH_WATER` before the flag is noticed.  With
///   `COOKED_BUF_SIZE = 4096` and `HIGH_WATER = 3072`, the worst-case
///   occupancy is ~3135 — well within the remaining 1024-byte headroom.
///   `push_cooked()` independently guards against actual overflow, so no
///   data loss occurs.
///
/// This is safe because `push_input()` sets `throttled = true` inside
/// the slot lock when the buffer reaches high-water, and the flag is
/// visible on the next batch boundary check.
pub fn master_write(peer: PtyPeerHandle, data: &[u8]) -> usize {
    if !validate_peer(&peer) {
        return 0;
    }
    let slave_slot = peer.idx.0 as usize;

    // Check throttle once before starting — if already throttled, return
    // a zero-length short write immediately.
    {
        let guard = TTY_SLOTS[slave_slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.throttled {
                return 0;
            }
        }
    }

    // Process bytes in batches.  After each batch, re-check the throttle
    // flag.  `push_input()` sets `throttled = true` inside the slot lock
    // when the cooked buffer reaches high-water, so the flag is visible
    // on the next batch boundary check.
    const BATCH_SIZE: usize = 64;
    let mut written = 0usize;

    for chunk in data.chunks(BATCH_SIZE) {
        for &byte in chunk {
            super::push_input(peer.idx, byte);
            written += 1;
        }

        // Re-check throttle after processing the batch.  If the slave
        // just became throttled, return a short write so the caller
        // blocks in the write() loop until the slave reader drains.
        {
            let guard = TTY_SLOTS[slave_slot].lock();
            if let Some(tty) = guard.as_ref() {
                if tty.throttled {
                    return written;
                }
            }
        }
    }

    written
}

/// Slave write → push bytes into the master's raw read buffer.
///
/// Phase 26: validates the peer handle's generation before writing.
/// If the peer slot was freed and potentially reused, the write is
/// silently discarded.
///
/// Returns the number of bytes successfully pushed into the master's
/// buffer.  Stops early when the master's input buffer is full,
/// preventing silent data loss from overflow.
pub fn slave_write(peer: PtyPeerHandle, data: &[u8]) -> usize {
    if !validate_peer(&peer) {
        return 0;
    }
    let slot = peer.idx.0 as usize;
    if slot >= MAX_TTYS {
        return 0;
    }

    let (written, should_wake) = {
        let mut guard = TTY_SLOTS[slot].lock();
        let Some(master) = guard.as_mut() else {
            return 0;
        };

        if master.peer_closed || master.hung_up {
            return 0;
        }

        let mut count = 0usize;
        for &byte in data {
            if master.ldisc.input_full() {
                break; // master buffer full — return short write
            }
            master.ldisc.input_char(byte);
            count += 1;
        }

        (count, master.ldisc.has_data())
    };

    if should_wake {
        TTY_INPUT_WAITERS[slot].wake_all();
    }
    written
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
        TtyDriverKind::PtyMaster { ref peer } => Ok(peer.idx.0 as u32),
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
// Pair lifecycle (Phase 20: atomic check-and-free, Phase 26: generation bump)
// ---------------------------------------------------------------------------

/// Free both sides of a PTY pair if both have `open_count == 0`.
///
/// Holds `PTY_ALLOC_LOCK` for the entire check-and-free sequence so that
/// no concurrent `pty_alloc()` or `pty_open_slave()` can observe a
/// partially freed pair (TOCTOU prevention).
///
/// Phase 26: increments `TTY_GENERATIONS` for each freed slot so that
/// any stale `PtyPeerHandle` referencing the old incarnation will fail
/// generation validation on subsequent writes.
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
    // Bump generation so stale PtyPeerHandles pointing at this slot are
    // invalidated.
    TTY_GENERATIONS[idx_slot].fetch_add(1, Ordering::Release);

    {
        let mut guard = TTY_SLOTS[peer_slot].lock();
        *guard = None;
    }
    TTY_GENERATIONS[peer_slot].fetch_add(1, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Phase 38: PTY slave lock management
// ---------------------------------------------------------------------------

/// Set the PTY slave lock state.
///
/// `idx` must refer to a **master** FD — the function resolves the
/// paired slave and sets its `slave_locked` flag.  Returns
/// `TtyError::NotAllocated` if `idx` is not a PTY master.
pub fn set_pty_lock(idx: TtyIndex, locked: bool) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Resolve the peer (slave) index from the master.
    let slave_idx = {
        let guard = TTY_SLOTS[slot].lock();
        let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
        match tty.driver {
            TtyDriverKind::PtyMaster { ref peer } => peer.idx,
            _ => return Err(TtyError::NotAllocated),
        }
    };

    // Set the lock on the slave.
    let slave_slot = slave_idx.0 as usize;
    if slave_slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let mut guard = TTY_SLOTS[slave_slot].lock();
    let tty = guard.as_mut().ok_or(TtyError::NotAllocated)?;
    tty.slave_locked = locked;
    Ok(())
}

/// Get the PTY slave lock state.
///
/// `idx` must refer to a **master** FD — the function resolves the
/// paired slave and reads its `slave_locked` flag.  Returns
/// `TtyError::NotAllocated` if `idx` is not a PTY master.
pub fn get_pty_lock(idx: TtyIndex) -> Result<bool, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Resolve the peer (slave) index from the master.
    let slave_idx = {
        let guard = TTY_SLOTS[slot].lock();
        let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
        match tty.driver {
            TtyDriverKind::PtyMaster { ref peer } => peer.idx,
            _ => return Err(TtyError::NotAllocated),
        }
    };

    // Read the lock from the slave.
    let slave_slot = slave_idx.0 as usize;
    if slave_slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slave_slot].lock();
    let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
    Ok(tty.slave_locked)
}

/// Check whether a PTY slave is currently locked.
///
/// Returns `true` if the slave at `idx` has `slave_locked == true`,
/// `false` if unlocked or the slot is not a PTY slave.
pub fn is_slave_locked(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => tty.slave_locked,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Phase 39: PTY packet mode
// ---------------------------------------------------------------------------

/// Enable or disable packet mode on a PTY master.
///
/// `idx` must refer to a **master** FD.  When packet mode is disabled,
/// any pending `packet_events` are cleared.
pub fn set_packet_mode(idx: TtyIndex, enable: bool) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let mut guard = TTY_SLOTS[slot].lock();
    let tty = guard.as_mut().ok_or(TtyError::NotAllocated)?;
    match tty.driver {
        TtyDriverKind::PtyMaster { .. } => {}
        _ => return Err(TtyError::NotAllocated),
    }

    tty.packet_mode = enable;
    if !enable {
        tty.packet_events = 0;
    }
    Ok(())
}

/// Get the current packet mode state of a PTY master.
pub fn get_packet_mode(idx: TtyIndex) -> Result<bool, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let guard = TTY_SLOTS[slot].lock();
    let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
    match tty.driver {
        TtyDriverKind::PtyMaster { .. } => Ok(tty.packet_mode),
        _ => Err(TtyError::NotAllocated),
    }
}

/// Queue packet event bits on the PTY master paired with a slave at `slave_idx`.
///
/// Resolves the master peer from the slave's driver, and ORs `event_bits`
/// into the master's `packet_events` if packet mode is enabled.
/// Wakes master readers and poll waiters so they see the pending events.
pub fn queue_packet_event(slave_idx: TtyIndex, event_bits: u8) {
    let slot = slave_idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    // Resolve the master peer index from the slave.
    let master_idx = {
        let guard = TTY_SLOTS[slot].lock();
        let Some(tty) = guard.as_ref() else { return };
        match tty.driver {
            TtyDriverKind::PtySlave { ref peer } => {
                if !validate_peer(peer) {
                    return;
                }
                peer.idx
            }
            _ => return,
        }
    };

    let master_slot = master_idx.0 as usize;
    if master_slot >= MAX_TTYS {
        return;
    }

    let should_wake = {
        let mut guard = TTY_SLOTS[master_slot].lock();
        let Some(master) = guard.as_mut() else {
            return;
        };
        if !master.packet_mode {
            return;
        }
        master.packet_events |= event_bits;
        true
    };

    if should_wake {
        TTY_INPUT_WAITERS[master_slot].wake_all();
        super::table::TTY_POLL_WAITERS[master_slot].wake_all();
    }
}
