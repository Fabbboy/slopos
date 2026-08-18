//! TTY job control: sessions, foreground process group, controlling-terminal
//! acquire/release/detach, SIGTTIN/SIGTTOU enforcement.
//!
//! Session and foreground-group links resolve to [`KWeak`] handles **before**
//! the per-TTY lock is taken: the resolvers acquire the task-manager lock, which
//! must never nest under a TTY slot lock.

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

/// Wakes blocked readers so they re-evaluate foreground status and take
/// `SIGTTIN` if they are now in the background.
#[must_use]
pub fn set_foreground_pgrp(idx: TtyIndex, pgid: u32) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    // An empty weak clears the foreground group.
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

/// POSIX `TIOCSPGRP`: only the TTY's controlling session may change the
/// foreground group, and the target group must have living members in it.
pub fn set_foreground_pgrp_checked(
    idx: TtyIndex,
    pgid: u32,
    caller_sid: u32,
) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

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
        BUS.publish(tty_input_event(slot));
    }
    Ok(())
}

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

/// The leader becomes the controlling process, its group the initial foreground.
pub fn attach_session(idx: TtyIndex, leader_pid: u32, leader_pgid: u32) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let session = session_handle(leader_pid).unwrap_or_else(KWeak::new);
    let fg = pgrp_handle(leader_pgid).unwrap_or_else(KWeak::new);
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.session.attach(session, fg);
    }
}

/// The controlling session is derived from the passed foreground group, which
/// pins it.
pub fn acquire_controlling_terminal(
    idx: TtyIndex,
    fg: KWeak<ProcessGroup>,
) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

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

/// Clears session and foreground pgrp; compositor focus is left alone.
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

/// `TIOCNOTTY`. From the session leader the whole session loses the terminal and
/// the foreground group takes SIGHUP + SIGCONT, per POSIX hangup behaviour;
/// otherwise only the caller detaches. `Ok(true)` means the leader path ran.
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
        // The ioctl handler clears the caller's own `controlling_tty`.
        return Ok(false);
    }

    // Pinning the group across the off-lock signal closes the reused-pid window.
    let (fg_pgrp, session_id) = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(tty) => tty,
            None => return Err(TtyError::NotAllocated),
        };

        let tty_sid = tty.session.session_id();
        if tty_sid != 0 && tty_sid != caller_sid {
            return Err(TtyError::PermissionDenied);
        }

        let pgrp = tty.session.fg_pgrp_handle();
        let sid = tty.session.session_id();
        tty.session.detach();
        (pgrp, sid)
    };

    // Signal delivery stays outside the lock.
    if session_id != 0 {
        let _ = clear_session_controlling_tty(session_id, idx);
    }
    if let Some(pgrp) = fg_pgrp {
        let _ = signal_process_group(pgrp.id(), SIGHUP);
        let _ = signal_process_group(pgrp.id(), SIGCONT);
    }

    Ok(true)
}
