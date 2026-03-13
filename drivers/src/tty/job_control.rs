//! TTY job control — session management, foreground process group,
//! controlling terminal acquire/release/detach, and SIGTTIN/SIGTTOU
//! enforcement.
//!
//! decomposition: extracted from `mod.rs` to isolate POSIX
//! job control operations from the I/O and termios paths.

use slopos_abi::signal::{SIGCONT, SIGHUP};

use slopos_lib::kernel_services::driver_runtime::{
    clear_session_controlling_tty, signal_process_group,
};

use super::table::TTY_SLOTS;
use super::{MAX_TTYS, TtyError, TtyIndex};

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
        Some(tty) => Ok(tty.session.fg_pgrp_raw()),
        None => Err(TtyError::NotAllocated),
    }
}

/// Set the foreground process group for a specific TTY.
#[must_use]
pub fn set_foreground_pgrp(idx: TtyIndex, pgid: u32) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let mut guard = TTY_SLOTS[slot].lock();
    match guard.as_mut() {
        Some(tty) => {
            tty.session.set_fg_pgrp_raw(pgid);
            Ok(())
        }
        None => Err(TtyError::NotAllocated),
    }
}

/// Set foreground pgrp with session validation (POSIX TIOCSPGRP semantics).
///
/// Only processes in the same session as the TTY's controlling session may
/// change the foreground pgrp.  Additionally validates that the
/// target process group actually has living members in the session.
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

    // Before acquiring the per-TTY lock, validate that the target
    // pgrp actually exists within the session.  This uses the scheduler's
    // task iterator and must be done outside the TTY lock.  Clearing the
    // foreground group (pgid == 0) is always allowed.
    if pgid != 0 {
        let guard = TTY_SLOTS[slot].lock();
        let session_id = match guard.as_ref() {
            Some(tty) if tty.session.has_session() => tty.session.session_id_raw(),
            Some(_) => 0, // no session attached — skip pgrp validation
            None => return Err(TtyError::NotAllocated),
        };
        drop(guard);

        if session_id != 0 {
            use slopos_lib::kernel_services::driver_runtime::pgrp_exists_in_session;
            if !pgrp_exists_in_session(pgid, session_id) {
                return Err(TtyError::PermissionDenied);
            }
        }
    }

    let mut guard = TTY_SLOTS[slot].lock();
    match guard.as_mut() {
        Some(tty) => {
            if tty.session.set_fg_pgrp_checked(pgid, caller_sid) {
                Ok(())
            } else {
                Err(TtyError::PermissionDenied)
            }
        }
        None => Err(TtyError::NotAllocated),
    }
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
        Some(tty) => Ok(tty.session.session_id_raw()),
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
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.session.attach(leader_pid, leader_pgid);
    }
}

pub fn acquire_controlling_terminal(
    idx: TtyIndex,
    session_leader: u32,
    session_pgid: u32,
) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let mut guard = TTY_SLOTS[slot].lock();
    let tty = match guard.as_mut() {
        Some(tty) => tty,
        None => return Err(TtyError::NotAllocated),
    };

    let current_sid = tty.session.session_id_raw();
    if current_sid != 0 && current_sid != session_leader {
        return Err(TtyError::PermissionDenied);
    }

    if current_sid == 0 {
        tty.session.attach(session_leader, session_pgid);
    }

    Ok(())
}

/// Detach the controlling session from a TTY.
///
/// Clears session leader, session ID, and foreground pgrp.
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

    if tty.session.session_id_raw() != session_id {
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
    // foreground process group.
    let (fg_pgid, session_id) = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(tty) => tty,
            None => return Err(TtyError::NotAllocated),
        };

        // Only the controlling session may detach.
        let tty_sid = tty.session.session_id_raw();
        if tty_sid != 0 && tty_sid != caller_sid {
            return Err(TtyError::PermissionDenied);
        }

        let pgid = tty.session.fg_pgrp_raw();
        let sid = tty.session.session_id_raw();
        tty.session.detach();
        (pgid, sid)
    };

    // Signal delivery OUTSIDE the lock to avoid deadlock.
    if session_id != 0 {
        let _ = clear_session_controlling_tty(session_id, idx);
    }
    if fg_pgid != 0 {
        let _ = signal_process_group(fg_pgid, SIGHUP);
        let _ = signal_process_group(fg_pgid, SIGCONT);
    }

    Ok(true)
}
