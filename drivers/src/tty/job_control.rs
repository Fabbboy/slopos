//! TTY job control — session management, foreground process group,
//! controlling terminal acquire/release/detach, and SIGTTIN/SIGTTOU
//! enforcement.
//!
//! Session and foreground-group links are resolved to weak handles
//! ([`KWeak`]) **before** the per-TTY lock is taken: the resolvers acquire the
//! task-manager lock, which must never nest under a TTY slot lock.

use slopos_abi::signal::{SIGCONT, SIGHUP};

use slopos_kernel_services::driver_runtime::{
    clear_session_controlling_tty, pgrp_handle, scheduler_is_enabled, session_handle,
    signal_process_group,
};

use super::table::{TTY_SLOTS, tty_input_event};
use super::{MAX_TTYS, TtyError, TtyIndex};
use slopos_ostd::sync::BUS;
use slopos_ostd::task::ProcessGroup;
use slopos_ostd::{KArc, KWeak};

// ---------------------------------------------------------------------------
// Foreground process group
// ---------------------------------------------------------------------------

/// Get the foreground process group for a specific TTY.
#[must_use]
pub fn get_foreground_pgrp(idx: TtyIndex) -> Result<u32, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => Ok(tty.session.fg_pgrp_id()),
        None => Err(TtyError::NotAllocated),
    }
}

/// Set the foreground process group for a specific TTY.
///
/// Wakes blocked readers so they re-evaluate foreground status and receive
/// `SIGTTIN` if they are now in the background.
#[must_use]
pub fn set_foreground_pgrp(idx: TtyIndex, pgid: u32) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    // Resolve the group handle off-lock (empty weak clears the foreground).
    let fg = if pgid == 0 {
        KWeak::new()
    } else {
        pgrp_handle(pgid).unwrap_or_else(KWeak::new)
    };
    {
        let mut guard = TTY_SLOTS[slot].lock();
        match guard.as_mut() {
            Some(tty) => {
                tty.session.set_fg_pgrp(fg);
            }
            None => return Err(TtyError::NotAllocated),
        }
    }

    if scheduler_is_enabled() != 0 {
        // Poll waiters park on the input queue too, so one publish covers both.
        BUS.publish(tty_input_event(slot));
    }
    Ok(())
}

/// Set foreground pgrp with session validation (POSIX TIOCSPGRP semantics).
///
/// Only processes in the same session as the TTY's controlling session may
/// change the foreground pgrp, and the target group must have living members
/// in that session.
///
/// Returns `Ok(())` on success, `Err(PermissionDenied)` on validation failure.
pub fn set_foreground_pgrp_checked(
    idx: TtyIndex,
    pgid: u32,
    caller_sid: u32,
) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Resolve the target group off-lock. Clearing (pgid == 0) is always
    // allowed; a named group with no living members cannot be foregrounded.
    let fg = if pgid == 0 {
        KWeak::new()
    } else {
        match pgrp_handle(pgid) {
            Some(w) => w,
            None => return Err(TtyError::PermissionDenied),
        }
    };

    let changed = {
        let mut guard = TTY_SLOTS[slot].lock();
        match guard.as_mut() {
            Some(tty) => {
                if tty.session.set_fg_pgrp_checked(fg, caller_sid) {
                    true
                } else {
                    return Err(TtyError::PermissionDenied);
                }
            }
            None => return Err(TtyError::NotAllocated),
        }
    };

    if changed && scheduler_is_enabled() != 0 {
        // Poll waiters park on the input queue too, so one publish covers both.
        BUS.publish(tty_input_event(slot));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Session management API
// ---------------------------------------------------------------------------

/// Get the session ID for a specific TTY.
#[must_use]
pub fn get_session_id(idx: TtyIndex) -> Result<u32, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => Ok(tty.session.session_id()),
        None => Err(TtyError::NotAllocated),
    }
}

/// Attach a session to a TTY.
///
/// The session leader (`leader_pid`) becomes the controlling process.
/// `leader_pgid` is set as the initial foreground process group.
pub fn attach_session(idx: TtyIndex, leader_pid: u32, leader_pgid: u32) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    // Resolve both handles off-lock before taking the TTY slot lock.
    let session = session_handle(leader_pid).unwrap_or_else(KWeak::new);
    let fg = pgrp_handle(leader_pgid).unwrap_or_else(KWeak::new);
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.session.attach(session, fg);
    }
}

/// Make the caller's session the controlling session of a TTY. The caller's
/// foreground group is passed as a weak handle; the session is derived from it
/// (a group pins its session).
pub fn acquire_controlling_terminal(
    idx: TtyIndex,
    fg: KWeak<ProcessGroup>,
) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // The acquiring session is the one that owns the caller's foreground group.
    let group = fg.upgrade();
    let acquiring_sid = group.as_ref().map_or(0, |pg| pg.session_id());
    let session = group
        .as_ref()
        .map_or_else(KWeak::new, |pg| KArc::downgrade(pg.session()));

    let mut guard = TTY_SLOTS[slot].lock();
    let tty = match guard.as_mut() {
        Some(tty) => tty,
        None => return Err(TtyError::NotAllocated),
    };

    // POSIX: PTY masters cannot become controlling terminals.
    if !tty.driver.can_be_controlling_terminal() {
        return Err(TtyError::PermissionDenied);
    }

    let current_sid = tty.session.session_id();
    if current_sid != 0 && current_sid != acquiring_sid {
        return Err(TtyError::PermissionDenied);
    }

    if current_sid == 0 {
        tty.session.attach(session, fg);
    }

    Ok(())
}

/// Detach the controlling session from a TTY.
///
/// Clears session and foreground pgrp.
/// Compositor focus (`focused_task_id`) is NOT cleared.
pub fn detach_session(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.session.detach();
    }
}

#[must_use]
pub fn release_controlling_terminal(idx: TtyIndex, session_id: u32) -> Result<bool, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let mut guard = TTY_SLOTS[slot].lock();
    let tty = match guard.as_mut() {
        Some(tty) => tty,
        None => return Err(TtyError::NotAllocated),
    };

    if tty.session.session_id() != session_id {
        return Ok(false);
    }

    tty.session.detach();
    Ok(true)
}

/// Detach the calling process from its controlling terminal
/// (TIOCNOTTY semantics).
///
/// If the caller is the session leader, the entire session loses the
/// controlling terminal — the foreground process group receives SIGHUP +
/// SIGCONT (matching POSIX hangup behavior).  The session is detached from
/// the TTY, and `clear_session_controlling_tty` clears every task in the
/// session that still refers to this TTY.
///
/// If the caller is NOT the session leader, only the caller's own
/// `controlling_tty` is cleared (the TTY session state is unaffected).
///
/// Returns `Ok(true)` if the caller was the session leader and signals
/// were sent, `Ok(false)` if only the caller was detached.
pub fn detach_controlling_terminal(
    idx: TtyIndex,
    caller_sid: u32,
    caller_is_session_leader: bool,
) -> Result<bool, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    if !caller_is_session_leader {
        // Non-leader: only the caller's controlling_tty field is cleared
        // (done by the ioctl handler).  TTY session state is unchanged.
        return Ok(false);
    }

    // Session leader: detach the session from the TTY and signal the
    // foreground process group. Pinning the group over the off-lock signal
    // keeps its identity stable (no reused-pid window).
    let (fg_pgrp, session_id) = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(tty) => tty,
            None => return Err(TtyError::NotAllocated),
        };

        // Only the controlling session may detach.
        let tty_sid = tty.session.session_id();
        if tty_sid != 0 && tty_sid != caller_sid {
            return Err(TtyError::PermissionDenied);
        }

        let pgrp = tty.session.fg_pgrp_handle();
        let sid = tty.session.session_id();
        tty.session.detach();
        (pgrp, sid)
    };

    // Signal delivery OUTSIDE the lock to avoid deadlock.
    if session_id != 0 {
        let _ = clear_session_controlling_tty(session_id, idx);
    }
    if let Some(pgrp) = fg_pgrp {
        let _ = signal_process_group(pgrp.id(), SIGHUP);
        let _ = signal_process_group(pgrp.id(), SIGCONT);
    }

    Ok(true)
}
