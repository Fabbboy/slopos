//! TTY lifecycle management — allocation, open/close reference counting,
//! hangup, vhangup, active TTY routing, and subsystem initialization.
//!
//! decomposition: extracted from `mod.rs` to isolate lifecycle
//! operations that control a TTY's existence and state transitions.

use core::sync::atomic::{AtomicU8, Ordering};

use slopos_abi::signal::{SIGCONT, SIGHUP};

use slopos_lib::kernel_services::driver_runtime::{
    clear_session_controlling_tty, scheduler_is_enabled, signal_session,
};

use super::driver::TtyDriverKind;
use super::pty;
use super::table::{TTY_INPUT_WAITERS, TTY_OUTPUT_WAITERS, TTY_POLL_WAITERS, TTY_SLOTS};
use super::{MAX_TTYS, TtyError, TtyIndex};

// ---------------------------------------------------------------------------
// Active TTY tracking (for keyboard input routing)
// ---------------------------------------------------------------------------

/// The currently active TTY index (receives keyboard input).
/// Defaults to 0 (serial console).
static ACTIVE_TTY: AtomicU8 = AtomicU8::new(0);
static DEFAULT_CONSOLE_TTY: AtomicU8 = AtomicU8::new(0);

/// Returns the TTY index that should receive keyboard input.
pub fn active_tty() -> TtyIndex {
    TtyIndex(ACTIVE_TTY.load(Ordering::Relaxed))
}

/// Set the active TTY (the one receiving keyboard input).
pub fn set_active_tty(idx: TtyIndex) {
    ACTIVE_TTY.store(idx.0, Ordering::Relaxed);
}

/// Switch keyboard routing to a specific active TTY.
///
/// This controls only the TTY input route (`active_tty`). It does not alter:
/// - compositor focus (UI/window focus)
/// - POSIX foreground process group/job control (`fg_pgrp`)
#[must_use]
pub fn switch_active_tty(idx: TtyIndex) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(tty) if tty.active => {}
            _ => return Err(TtyError::NotAllocated),
        }
    }

    set_active_tty(idx);
    if scheduler_is_enabled() != 0 {
        TTY_INPUT_WAITERS[slot].wake_all();
        TTY_POLL_WAITERS[slot].wake_all();
    }
    Ok(())
}

pub fn set_default_console_tty(idx: TtyIndex) {
    DEFAULT_CONSOLE_TTY.store(idx.0, Ordering::Relaxed);
}

pub fn default_console_tty() -> TtyIndex {
    TtyIndex(DEFAULT_CONSOLE_TTY.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Subsystem initialisation
// ---------------------------------------------------------------------------

/// Initialise the TTY subsystem.  Call during early boot after serial is ready.
pub fn init() {
    super::table::tty_table_init();
}

// ---------------------------------------------------------------------------
// Open / close reference counting
// ---------------------------------------------------------------------------

#[must_use]
pub fn open_ref(idx: TtyIndex) -> Result<u32, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        let peer_to_reopen = match tty.driver {
            TtyDriverKind::PtySlave { ref peer } => Some(peer.idx),
            _ => None,
        };
        tty.open_count = tty
            .open_count
            .checked_add(1)
            .unwrap_or_else(|| panic!("tty open_count overflow for idx {}", idx.0));
        tty.hung_up = false;
        tty.peer_closed = false;
        let open_count = tty.open_count;
        drop(guard);

        if let Some(peer_idx) = peer_to_reopen {
            pty::clear_peer_closed(peer_idx);
        }

        return Ok(open_count);
    }
    Err(TtyError::NotAllocated)
}

#[must_use]
pub fn close_ref(idx: TtyIndex) -> Result<u32, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        if tty.open_count == 0 {
            return Ok(0);
        }
        tty.open_count -= 1;
        let open_count = tty.open_count;
        if tty.open_count == 0 {
            match tty.driver {
                TtyDriverKind::PtyMaster { ref peer } => {
                    let slave_idx = peer.idx;
                    drop(guard);
                    hangup(slave_idx);
                    pty::free_pair_if_unused(idx, slave_idx);
                    return Ok(0);
                }
                TtyDriverKind::PtySlave { ref peer } => {
                    let master_idx = peer.idx;
                    drop(guard);
                    pty::mark_peer_closed(master_idx);
                    pty::free_pair_if_unused(idx, master_idx);
                    return Ok(0);
                }
                TtyDriverKind::SerialConsole(_)
                | TtyDriverKind::VConsole(_)
                | TtyDriverKind::None => {
                    tty.ldisc.flush_all();
                    tty.session.detach();
                    tty.hung_up = false;
                    tty.peer_closed = false;
                }
            }
        }
        return Ok(open_count);
    }
    Err(TtyError::NotAllocated)
}

// ---------------------------------------------------------------------------
// Hangup
// ---------------------------------------------------------------------------

pub fn hangup(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let session_id = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(t) => t,
            None => return,
        };
        let sid = tty.session.session_id_raw();
        tty.ldisc.flush_all();
        // Clear output stop so blocked writers unblock
        // and see the hung_up flag on re-check.
        tty.output_stopped = false;
        // full flush → both FLUSHREAD + FLUSHWRITE packet events.
        // Deferred until after lock is dropped to avoid self-deadlock.
        tty.session.detach();
        tty.hung_up = true;
        sid
    };

    // Deliver deferred packet events now that the slot lock is released.
    pty::queue_packet_event(
        idx,
        slopos_abi::syscall::TIOCPKT_FLUSHREAD | slopos_abi::syscall::TIOCPKT_FLUSHWRITE,
    );

    // Signal the entire session (not just fg_pgrp) so that all
    // processes in the session receive SIGHUP + SIGCONT on hangup.
    if session_id != 0 {
        let _ = clear_session_controlling_tty(session_id, idx);
        let _ = signal_session(session_id, SIGHUP);
        let _ = signal_session(session_id, SIGCONT);
    }

    if scheduler_is_enabled() != 0 {
        TTY_INPUT_WAITERS[slot].wake_all();
        // Wake output waiters and per-slot poll sleepers
        // so they see POLLHUP.
        TTY_OUTPUT_WAITERS[slot].wake_all();
        TTY_POLL_WAITERS[slot].wake_all();
    }
}

pub fn is_hung_up(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => tty.hung_up,
        None => false,
    }
}

/// Revoke access to the caller's controlling terminal.
///
/// This is the kernel-side implementation of the `vhangup()` syscall.
/// It reuses the existing hangup infrastructure to:
/// - Flush buffers and detach the session
/// - Mark the TTY as hung up so subsequent I/O returns EIO
/// - Signal the session with SIGHUP + SIGCONT
/// - Wake all blocked readers/writers
///
/// The caller must provide their controlling terminal index.  Permission
/// checks (caller has a ctty) are enforced by the syscall handler.
pub fn vhangup(idx: TtyIndex) {
    hangup(idx);
}
