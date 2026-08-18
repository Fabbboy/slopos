//! PTY pair allocation and data routing.
//!
//! Pair lifetime lives in [`super::backing::TtyBacking`]: the master backing
//! owns the slave strongly, the slave holds the master weakly, and each end's
//! `Drop` frees its own slot. Cross-end references are `KWeak<TtyBacking>`
//! links; a data path upgrades one first, which pins the peer's slot for the
//! operation, so slot reuse can never misroute.
//!
//! Lock ordering: `PTY_ALLOC_LOCK` → `TTY_SLOTS[i]`, never the reverse.
//! `PTY_ALLOC_LOCK` is not held on data paths, and backing `Drop` bodies never
//! take it.

use slopos_ostd::lock_class;
use slopos_ostd::process::AccountId;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};
use slopos_ostd::{KArc, KWeak};

use super::backing::TtyBacking;
use super::driver::{InputEvent, TtyDriverKind};
use super::output::WriteNesting;
use super::table::{
    TTY_BACKINGS, TTY_SLOTS, find_free_slot, find_free_slot_excluding, mark_slot_allocated,
    tty_input_event,
};
use super::{MAX_TTYS, PacketEvents, Tty, TtyError, TtyFlags, TtyIndex};
use slopos_ostd::sync::BUS;

/// Serialises [`pty_alloc`]: find-two-free-slots + initialise must be atomic
/// against other allocators. Frees need no lock — a slot's backing `Drop` is
/// its sole freer and clears the bitmap bit only once the slot is empty.
static PTY_ALLOC_LOCK: SpinLock<()> =
    SpinLock::new((), lock_class!("PTY_ALLOC_LOCK", LOCK_LEVEL_REGISTRY));

/// Allocate a PTY master/slave pair and return the master's owning
/// backing (the `/dev/ptmx` open). The slave end is created alongside it,
/// kept alive by the master's strong link, and opened separately via
/// `pty_open_slave` / `pty_open_peer`.
pub fn pty_alloc(account: AccountId) -> Result<(TtyIndex, KArc<TtyBacking>), TtyError> {
    let _alloc = PTY_ALLOC_LOCK.lock();

    let master_slot = find_free_slot().ok_or(TtyError::NotAllocated)?;
    let slave_slot = find_free_slot_excluding(master_slot).ok_or(TtyError::NotAllocated)?;

    let master_idx = TtyIndex(master_slot as u8);
    let slave_idx = TtyIndex(slave_slot as u8);

    let (master_backing, slave_backing) =
        TtyBacking::new_pair(master_idx, slave_idx, account).ok_or(TtyError::OutOfMemory)?;

    // Both states are built before either is installed, so a mid-way
    // allocation failure leaves the slots untouched.
    let master = Tty::new_pty_master(master_idx, KArc::downgrade(&slave_backing))
        .map_err(|_| TtyError::OutOfMemory)?;
    let slave = Tty::new_pty_slave(slave_idx, KArc::downgrade(&master_backing))
        .map_err(|_| TtyError::OutOfMemory)?;

    {
        let mut guard = TTY_SLOTS[master_slot].lock();
        *guard = Some(master);
    }
    {
        let mut guard = TTY_SLOTS[slave_slot].lock();
        *guard = Some(slave);
    }
    *TTY_BACKINGS[master_slot].lock() = KArc::downgrade(&master_backing);
    *TTY_BACKINGS[slave_slot].lock() = KArc::downgrade(&slave_backing);
    mark_slot_allocated(master_slot);
    mark_slot_allocated(slave_slot);

    Ok((master_idx, master_backing))
}

/// Master write → push bytes into the slave's line discipline input.
///
/// Returns the number of bytes pushed, stopping short once the slave's cooked
/// buffer reaches the throttle high-water mark; the caller retries the
/// remainder. A failed peer upgrade means the pair is gone.
///
/// Throttle is sampled once per 64-byte batch, not per byte, which would cost
/// a slot-lock cycle per byte. The batch may therefore overshoot
/// `THROTTLE_HIGH_WATER` by up to 63 bytes, inside the 2048 bytes of headroom
/// `COOKED_BUF_SIZE` leaves; `push_cooked()` guards actual overflow.
pub fn master_write(peer: &KWeak<TtyBacking>, data: &[u8]) -> usize {
    let Some(slave_pin) = peer.upgrade() else {
        return 0;
    };
    if data.is_empty() {
        return 0;
    }
    let slave_idx = slave_pin.index();
    let slave_slot = slave_idx.0 as usize;

    // While throttled, one ldisc-priority control byte (VINTR/VQUIT/VSUSP under
    // ISIG, VSTART/VSTOP under IXON) still enters, so job-control signals are
    // not stuck behind back-pressured input.
    let allow_single_priority = {
        let guard = TTY_SLOTS[slave_slot].lock();
        let Some(tty) = guard.as_ref() else {
            return 0;
        };
        if tty.flags.contains(TtyFlags::THROTTLED) {
            if tty.flags.contains(TtyFlags::HUNG_UP) || tty.flags.contains(TtyFlags::PEER_CLOSED) {
                return 0;
            }
            if tty.ldisc.priority_control_input(data[0]) {
                true
            } else {
                return 0;
            }
        } else {
            false
        }
    };

    if allow_single_priority {
        let event = InputEvent::normal(data[0]);
        super::io::push_input_batch_nested(
            slave_idx,
            core::slice::from_ref(&event),
            WriteNesting::PeerNested,
        );
        return 1;
    }

    const BATCH_SIZE: usize = 64;
    let mut written = 0usize;

    for chunk in data.chunks(BATCH_SIZE) {
        let mut events = [InputEvent::normal(0); BATCH_SIZE];
        for (i, &byte) in chunk.iter().enumerate() {
            events[i] = InputEvent::normal(byte);
        }
        super::io::push_input_batch_nested(
            slave_idx,
            &events[..chunk.len()],
            WriteNesting::PeerNested,
        );
        written += chunk.len();

        {
            let guard = TTY_SLOTS[slave_slot].lock();
            if let Some(tty) = guard.as_ref() {
                if tty.flags.contains(TtyFlags::THROTTLED) {
                    return written;
                }
            }
        }
    }

    written
}

/// Slave write → push bytes into the master's raw read buffer.
///
/// Returns the number of bytes pushed, stopping short when the master's input
/// buffer is full. A failed peer upgrade means the master is gone.
pub fn slave_write(peer: &KWeak<TtyBacking>, data: &[u8]) -> usize {
    let Some(master_pin) = peer.upgrade() else {
        return 0;
    };
    let slot = master_pin.index().0 as usize;

    let (written, should_wake) = {
        let mut guard = TTY_SLOTS[slot].lock();
        let Some(master) = guard.as_mut() else {
            return 0;
        };

        if master.flags.contains(TtyFlags::PEER_CLOSED) || master.flags.contains(TtyFlags::HUNG_UP)
        {
            return 0;
        }

        // A reader parks only while `has_data()` is false, so the false→true
        // edge is the one wake that cannot be skipped: `should_wake_reader`
        // alone needs `WAKEUP_CHARS` bytes and would sleep through a lone
        // flow-control byte.
        let was_idle = !master.ldisc.has_data();

        let mut count = 0usize;
        for &byte in data {
            if master.ldisc.input_full() {
                break;
            }
            master.ldisc.input_char(InputEvent::normal(byte));
            count += 1;
        }

        let woke = master.ldisc.should_wake_reader();
        (count, woke || (was_idle && master.ldisc.has_data()))
    };

    if should_wake {
        BUS.publish(tty_input_event(slot));
    }
    written
}

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
    match &tty.driver {
        TtyDriverKind::PtyMaster { peer } => match peer.upgrade() {
            Some(slave) => Ok(slave.index().0 as u32),
            None => Err(TtyError::NotAllocated),
        },
        _ => Err(TtyError::NotAllocated),
    }
}

/// Latch `PEER_CLOSED` on `idx` and wake its readers/poll waiters so they
/// observe EOF. Fired from the slave backing's `Drop` against the master.
pub(crate) fn peer_closed(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.flags.insert(TtyFlags::PEER_CLOSED);
    }
    drop(guard);
    BUS.publish(tty_input_event(slot));
}

pub(crate) fn clear_peer_closed(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.flags.remove(TtyFlags::PEER_CLOSED);
    }
}

/// Set the PTY slave lock state. `idx` must refer to a **master** FD; the
/// paired slave's flag is what changes. `TtyError::NotAllocated` if `idx` is
/// not a PTY master.
pub fn set_pty_lock(idx: TtyIndex, locked: bool) -> Result<(), TtyError> {
    let slave = resolve_slave_of_master(idx)?;
    let mut guard = TTY_SLOTS[slave.index().0 as usize].lock();
    let tty = guard.as_mut().ok_or(TtyError::NotAllocated)?;
    tty.flags.set(TtyFlags::SLAVE_LOCKED, locked);
    Ok(())
}

/// Get the PTY slave lock state. `idx` must refer to a **master** FD; the
/// paired slave's flag is what is read. `TtyError::NotAllocated` if `idx` is
/// not a PTY master.
pub fn get_pty_lock(idx: TtyIndex) -> Result<bool, TtyError> {
    let slave = resolve_slave_of_master(idx)?;
    let guard = TTY_SLOTS[slave.index().0 as usize].lock();
    let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
    Ok(tty.flags.contains(TtyFlags::SLAVE_LOCKED))
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
        Some(tty) => tty.flags.contains(TtyFlags::SLAVE_LOCKED),
        None => false,
    }
}

/// Resolve (and pin) the slave backing paired with the master at `idx`.
fn resolve_slave_of_master(idx: TtyIndex) -> Result<KArc<TtyBacking>, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slot].lock();
    let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
    match &tty.driver {
        TtyDriverKind::PtyMaster { peer } => peer.upgrade().ok_or(TtyError::NotAllocated),
        _ => Err(TtyError::NotAllocated),
    }
}

// ---------------------------------------------------------------------------
// PTY packet mode
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

    tty.flags.set(TtyFlags::PACKET_MODE, enable);
    if !enable {
        tty.packet_events = PacketEvents::empty();
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
        TtyDriverKind::PtyMaster { .. } => Ok(tty.flags.contains(TtyFlags::PACKET_MODE)),
        _ => Err(TtyError::NotAllocated),
    }
}

/// Queue packet event bits on the PTY master paired with a slave at `slave_idx`.
///
/// Resolves and pins the master through the slave's peer link, and ORs
/// `event_bits` into the master's `packet_events` if packet mode is
/// enabled. Wakes master readers and poll waiters so they see the pending
/// events.
pub fn queue_packet_event(slave_idx: TtyIndex, event_bits: u8) {
    let slot = slave_idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    // Resolve and pin the master through the slave's peer link.
    let master_pin = {
        let guard = TTY_SLOTS[slot].lock();
        let Some(tty) = guard.as_ref() else { return };
        match &tty.driver {
            TtyDriverKind::PtySlave { peer } => match peer.upgrade() {
                Some(pin) => pin,
                None => return,
            },
            _ => return,
        }
    };

    let master_slot = master_pin.index().0 as usize;
    let should_wake = {
        let mut guard = TTY_SLOTS[master_slot].lock();
        let Some(master) = guard.as_mut() else {
            return;
        };
        if !master.flags.contains(TtyFlags::PACKET_MODE) {
            return;
        }
        master.packet_events |= PacketEvents::from_bits_truncate(event_bits);
        true
    };

    if should_wake {
        // Poll waiters park on the input queue too, so one publish covers both.
        BUS.publish(tty_input_event(master_slot));
    }
}
