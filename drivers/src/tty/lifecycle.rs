//! Hangup, vhangup, active-TTY routing and subsystem init. Open/close lifetime
//! is elsewhere: cloning and dropping `KArc<TtyBacking>` in [`super::backing`]
//! is the only open/close mechanism.

use core::sync::atomic::{AtomicU8, Ordering};

use slopos_abi::signal::{SIGCONT, SIGHUP};

use slopos_kernel_services::driver_runtime::{
    clear_session_controlling_tty, scheduler_is_enabled, signal_session,
};

use super::pty;
use super::table::{TTY_SLOTS, tty_input_event, tty_output_event};
use super::{MAX_TTYS, TtyError, TtyFlags, TtyIndex};
use slopos_ostd::sync::BUS;

/// The TTY receiving keyboard input; index 0 is the serial console.
static ACTIVE_TTY: AtomicU8 = AtomicU8::new(0);
static DEFAULT_CONSOLE_TTY: AtomicU8 = AtomicU8::new(0);

pub fn active_tty() -> TtyIndex {
    TtyIndex(ACTIVE_TTY.load(Ordering::Relaxed))
}

pub fn set_active_tty(idx: TtyIndex) {
    ACTIVE_TTY.store(idx.0, Ordering::Relaxed);
}

/// Switches the TTY input route only — not compositor focus, not the POSIX
/// foreground process group.
#[must_use]
pub fn switch_active_tty(idx: TtyIndex) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(_) => {}
            _ => return Err(TtyError::NotAllocated),
        }
    }

    set_active_tty(idx);
    if scheduler_is_enabled() != 0 {
        // Poll waiters park on the input queue too, so one publish covers both.
        BUS.publish(tty_input_event(slot));
    }
    Ok(())
}

pub fn set_default_console_tty(idx: TtyIndex) {
    DEFAULT_CONSOLE_TTY.store(idx.0, Ordering::Relaxed);
}

pub fn default_console_tty() -> TtyIndex {
    TtyIndex(DEFAULT_CONSOLE_TTY.load(Ordering::Relaxed))
}

/// Call during early boot, after serial is ready.
pub fn init() {
    super::table::tty_table_init();
}

pub fn hangup(idx: TtyIndex) {
    let Some(session_id) = hangup_mark(idx) else {
        return;
    };
    hangup_notify(idx, session_id);
}

/// The locked half of a hangup; returns the detached session id for
/// [`hangup_notify`], or `None` when the slot is empty. Split so a caller
/// serialising against reopen can hold its own outer lock across this half
/// while signals and wakeups stay outside every lock.
pub(crate) fn hangup_mark(idx: TtyIndex) -> Option<u32> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return None;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    let tty = guard.as_mut()?;
    let sid = tty.session.session_id();
    tty.ldisc.flush_all();
    // The resulting packet events are deferred past the lock drop: emitting them
    // here would self-deadlock.
    tty.session.detach();
    tty.mark_hung_up();
    Some(sid)
}

/// The unlocked half of a hangup, run once the slot lock is released.
pub(crate) fn hangup_notify(idx: TtyIndex, session_id: u32) {
    let slot = idx.0 as usize;

    pty::queue_packet_event(
        idx,
        slopos_abi::syscall::TIOCPKT_FLUSHREAD | slopos_abi::syscall::TIOCPKT_FLUSHWRITE,
    );

    // The whole session, not just fg_pgrp: every process in it gets the pair.
    if session_id != 0 {
        let _ = clear_session_controlling_tty(session_id, idx);
        let _ = signal_session(session_id, SIGHUP);
        let _ = signal_session(session_id, SIGCONT);
    }

    if scheduler_is_enabled() != 0 {
        // Poll waiters park on both queues; readers, writers and poll waiters
        // must all get the chance to observe POLLHUP.
        BUS.publish(tty_input_event(slot));
        BUS.publish(tty_output_event(slot));
    }
}

/// Return a hung-up slot to service. A hangup is terminal in production, so the
/// only caller is a test that owes the rest of the boot a working console.
/// Confined to the slot lock, so it is safe while idle CPUs sweep the table.
#[cfg(feature = "test-hooks")]
pub fn clear_hangup(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.flags.remove(TtyFlags::HUNG_UP | TtyFlags::PEER_CLOSED);
        tty.ldisc.flush_all();
    }
}

pub fn is_hung_up(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => tty.flags.contains(TtyFlags::HUNG_UP),
        None => false,
    }
}

/// Kernel side of `vhangup()`. `idx` must be the caller's controlling terminal;
/// the syscall handler is what checks that it has one.
pub fn vhangup(idx: TtyIndex) {
    hangup(idx);
}

#[must_use]
pub fn set_exclusive(idx: TtyIndex, enable: bool) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let mut guard = TTY_SLOTS[slot].lock();
    match guard.as_mut() {
        Some(tty) => {
            if enable {
                tty.flags.insert(TtyFlags::EXCLUSIVE);
            } else {
                tty.flags.remove(TtyFlags::EXCLUSIVE);
            }
            Ok(())
        }
        None => Err(TtyError::NotAllocated),
    }
}

#[must_use]
pub fn get_exclusive(idx: TtyIndex) -> Result<bool, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => Ok(tty.flags.contains(TtyFlags::EXCLUSIVE)),
        None => Err(TtyError::NotAllocated),
    }
}
