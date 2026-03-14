//! TTY poll readiness and compositor focus management.
//!
//! decomposition: extracted from `mod.rs` to isolate event-driven
//! poll readiness computation (`poll_events`), multi-slot poll sleep
//! (`poll_sleep_on`, `poll_sleep`), and compositor-level focus tracking
//! (`set_compositor_focus`, `get_compositor_focus`).

use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT};

use slopos_lib::kernel_services::driver_runtime::scheduler_is_enabled;

use super::table::{TTY_INPUT_WAITERS, TTY_POLL_WAITERS, TTY_SLOTS};
use super::{MAX_TTYS, PostLockWork, TtyError, TtyFlags, TtyIndex};

// ---------------------------------------------------------------------------
// Compositor focus
// ---------------------------------------------------------------------------

/// Set the compositor-level focus on the active TTY.
///
/// Called by the compositor when window focus changes.  Sets ONLY the
/// `focused_task_id` — it does NOT alter the POSIX foreground process
/// group (`fg_pgrp`).  The two concepts are independent:
///
/// - `focused_task_id` — which task the compositor considers "active"
/// - `fg_pgrp` — which process group may read/write the terminal (POSIX)
///
/// Compositor focus is used for input routing; foreground pgrp is used
/// for job control signals and read/write access gating.
#[must_use]
pub fn set_compositor_focus(task_id: u32) -> Result<(), TtyError> {
    let idx = super::active_tty();
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let mut found = false;
    {
        let mut guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_mut() {
            tty.session.focused_task_id = task_id;
            found = true;
        }
    }
    if !found {
        return Err(TtyError::NotAllocated);
    }
    if scheduler_is_enabled() != 0 {
        TTY_INPUT_WAITERS[slot].wake_all();
    }
    Ok(())
}

/// Get the compositor-focused task ID from the active TTY.
#[must_use]
pub fn get_compositor_focus() -> Result<u32, TtyError> {
    let idx = super::active_tty();
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => Ok(tty.session.focused_task_id),
        None => Err(TtyError::NotAllocated),
    }
}

// ---------------------------------------------------------------------------
// Event-driven poll readiness
// ---------------------------------------------------------------------------

/// Compute poll readiness events for a TTY file descriptor.
///
/// Drains pending hardware input, then checks:
/// - `POLLIN`  — cooked data available for reading
/// - `POLLOUT` — output is NOT stopped by IXON flow control
/// - `POLLHUP` — TTY is hung up (or peer closed with no remaining data)
/// - `POLLERR` — TTY is hung up (write would return EIO); matches Linux
///   `tty_poll()` which reports `POLLERR` alongside `POLLHUP` so programs
///   that check write-readiness via `POLLERR` detect the error condition.
///
/// Properly captures and delivers deferred signals from
/// `drain_hw_input_locked()` instead of silently discarding them.
///
/// Only events that are both requested and ready are returned.
pub fn poll_events(idx: TtyIndex, requested: u16) -> u16 {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return 0;
    }

    let mut deferred = PostLockWork::new();
    let revents = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(t) => t,
            None => return 0,
        };

        if let Some((pgid, sig)) = tty.drain_hw_input_locked() {
            deferred.add_signal(pgid, sig);
        }

        let mut revents = 0u16;

        if (requested & POLLIN) != 0
            && (tty.ldisc.has_data()
                || (tty.flags.contains(TtyFlags::PACKET_MODE) && !tty.packet_events.is_empty()))
        {
            revents |= POLLIN;
        }

        if (requested & POLLOUT) != 0
            && !tty.ldisc.is_stopped()
            && !tty.flags.contains(TtyFlags::OUTPUT_STOPPED)
        {
            revents |= POLLOUT;
        }

        if tty.flags.contains(TtyFlags::HUNG_UP)
            || (tty.flags.contains(TtyFlags::PEER_CLOSED) && !tty.ldisc.has_data())
        {
            revents |= POLLHUP | POLLERR;
            if (requested & POLLIN) != 0 {
                revents |= POLLIN;
            }
        }

        revents
    };

    deferred.execute();
    revents
}

/// Sleep until a poll-relevant event occurs on one of the given TTY slots,
/// or fall back to a short busy-wait if the scheduler is not yet enabled.
///
/// Per-slot registration.  The caller provides the set
/// of TTY indices it is currently monitoring.  The current task is enqueued
/// on each slot's `TTY_POLL_WAITERS` entry, then blocked exactly once.
/// When *any* of the registered slots fires a wake, the task resumes and
/// is cleaned up from all queues.
///
/// If `slots` is empty, falls back to a 1 ms delay (timer poll).
pub fn poll_sleep_on(slots: &[u8]) {
    if scheduler_is_enabled() == 0 {
        // Pre-scheduler fallback: yield briefly.
        slopos_lib::kernel_services::platform::timer_poll_delay_ms(1);
        return;
    }

    if slots.is_empty() {
        slopos_lib::kernel_services::platform::timer_poll_delay_ms(1);
        return;
    }

    // Enqueue the current task on each slot's poll waiter.
    let mut registered = 0usize;
    for &slot in slots {
        let s = slot as usize;
        if s < MAX_TTYS && TTY_POLL_WAITERS[s].enqueue_current() {
            registered += 1;
        }
    }

    if registered == 0 {
        // Could not enqueue on any queue — fall back to brief delay.
        slopos_lib::kernel_services::platform::timer_poll_delay_ms(1);
        return;
    }

    // Block once — any wake from any registered queue unblocks us.
    slopos_lib::kernel_services::driver_runtime::block_current_task();

    // Clean up: remove ourselves from all registered queues.
    for &slot in slots {
        let s = slot as usize;
        if s < MAX_TTYS {
            TTY_POLL_WAITERS[s].remove_current();
        }
    }
}

/// Legacy poll_sleep with no slot information — falls back to sleeping on
/// ALL active TTY poll waiters.  Retained for backward compatibility with
/// code paths that do not yet pass slot indices.
pub fn poll_sleep() {
    let mut slots = [0u8; MAX_TTYS];
    let mut count = 0;
    let mut bits = super::table::active_slots_bitmap();
    while bits != 0 {
        let i = bits.trailing_zeros() as usize;
        slots[count] = i as u8;
        count += 1;
        bits &= bits - 1;
    }
    poll_sleep_on(&slots[..count]);
}
