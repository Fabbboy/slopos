//! TTY I/O paths — read, write, push_input, hardware drain, data queries,
//! and the idle-loop input callback.
//!
//! decomposition: extracted from `mod.rs` to isolate the hot data
//! paths (read/write/push_input) from termios configuration, lifecycle
//! management, job control, and poll readiness.

use core::ffi::c_int;
use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIGTTIN, SIGTTOU};
use slopos_abi::syscall::LocalFlags;

use slopos_lib::kernel_services::driver_runtime::{
    current_task_id, current_task_pgid, current_task_sid, has_pending_signal,
    is_current_signal_blocked_or_ignored, is_pgrp_orphaned, register_idle_wakeup_callback,
    scheduler_is_enabled, signal_process_group,
};

use super::driver::{write_driver_unlocked, InputEvent, TtyDriverKind};
use super::ldisc::{self, BatchResult, OutputAction};
use super::pty;
use super::session::ForegroundCheck;
use super::table::{
    TTY_INPUT_WAITERS, TTY_OUTPUT_INFLIGHT, TTY_OUTPUT_WAITERS, TTY_POLL_WAITERS, TTY_SLOTS,
};
use super::{Tty, TtyError, TtyIndex, MAX_TTYS};

// ---------------------------------------------------------------------------
// Tty helper method — hardware drain
// ---------------------------------------------------------------------------

impl Tty {
    /// Drain pending hardware input into the line discipline.
    ///
    /// Called while holding the per-TTY lock.  Feeds bytes from the hardware
    /// driver through `ldisc.input_char()`, echoing output via the driver.
    ///
    /// Returns a deferred signal `(pgid, signum)` if signal generation was
    /// triggered (e.g. Ctrl+C on serial).  The caller **must** deliver the
    /// signal **after** dropping the per-TTY lock to avoid deadlock.
    pub(crate) fn drain_hw_input_locked(&mut self) -> Option<(u32, u8)> {
        let mut scratch = [0u8; 64];
        let count = self.driver.drain_input(&mut scratch);
        let mut events = [InputEvent::normal(0); 64];
        for i in 0..count {
            let mut c = scratch[i];
            if c == b'\r' {
                c = b'\n';
            } else if c == 0x7F {
                c = 0x08;
            }
            events[i] = InputEvent::normal(c);
        }

        let batch = self.ldisc.receive_buf(&events[..count]);
        if batch.echo_len > 0 {
            self.driver.write_output(&batch.echo[..batch.echo_len]);
        }
        if let Some(xoff) = self.ldisc.ixoff_check_xoff() {
            self.driver.write_output(&[xoff]);
        }
        batch
            .signal
            .map(|(sig, _)| (self.session.fg_pgrp_raw(), sig))
    }
}

// ---------------------------------------------------------------------------
// PTY re-exports (public API surface)
// ---------------------------------------------------------------------------

pub use super::pty::{
    get_packet_mode, get_pty_lock, get_pty_number, is_pty_slave, is_slave_locked, pty_alloc,
    pty_open_slave, queue_packet_event, set_packet_mode, set_pty_lock,
};

// ---------------------------------------------------------------------------
// Input push (from ISR / PTY master write)
// ---------------------------------------------------------------------------

/// Push a raw input byte to a specific TTY.
///
/// Called from interrupt context (keyboard ISR) or from `drain_hw_input_locked`.
/// Feeds the byte through the line discipline and handles echo/signal actions.
pub fn push_input<E: Into<InputEvent>>(idx: TtyIndex, event: E) {
    let event = event.into();
    push_input_batch(idx, core::slice::from_ref(&event));
}

pub fn push_input_batch(idx: TtyIndex, events: &[InputEvent]) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS || events.is_empty() {
        return;
    }

    let mut route: Option<(super::driver::DriverId, [u8; 256], usize)> = None;
    let mut deferred_signal = None;
    let mut ixoff_byte_out = None;
    let mut output_resumed = false;
    let mut deferred_pkt_event: u8 = 0;
    let wake = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(t) => t,
            None => return,
        };

        if tty.hung_up {
            return;
        }

        // Track stopped state before input processing so we
        // can detect stopped→resumed transitions for IXON wakeup.
        let was_stopped = tty.ldisc.is_stopped();

        let batch: BatchResult = tty.ldisc.receive_buf(events);
        if let Some(xoff) = tty.ldisc.ixoff_check_xoff() {
            ixoff_byte_out = Some((tty.driver.id(), xoff));
        }

        // If output transitioned from stopped to resumed,
        // wake blocked writers and poll waiters.
        if was_stopped && !tty.ldisc.is_stopped() {
            output_resumed = true;
        }

        // Track flow-control transitions for packet mode.
        // Deferred: queue_packet_event acquires its own TTY slot lock,
        // so we must not call it while holding `guard`.
        if !was_stopped && tty.ldisc.is_stopped() {
            deferred_pkt_event = slopos_abi::syscall::TIOCPKT_STOP;
        } else if was_stopped && !tty.ldisc.is_stopped() {
            deferred_pkt_event = slopos_abi::syscall::TIOCPKT_START;
        }

        // Activate throttle when cooked buffer fills.
        if batch.throttle_check
            && !tty.throttled
            && tty.ldisc.bytes_available() >= ldisc::THROTTLE_HIGH_WATER
        {
            tty.throttled = true;
        }

        if batch.echo_len > 0 {
            let mut out = [0u8; 256];
            out[..batch.echo_len].copy_from_slice(&batch.echo[..batch.echo_len]);
            route = Some((tty.driver.id(), out, batch.echo_len));
        }

        if let Some((sig, _)) = batch.signal {
            deferred_signal = Some((tty.session.fg_pgrp_raw(), sig));
        }

        batch.should_wake
    };

    // Deliver deferred packet event now that the slot lock is released.
    if deferred_pkt_event != 0 {
        pty::queue_packet_event(idx, deferred_pkt_event);
    }

    if let Some((driver_id, out, out_len)) = route {
        // Track in-flight echo output for drain semantics.
        TTY_OUTPUT_INFLIGHT[slot].fetch_add(1, Ordering::Release);
        write_driver_unlocked(driver_id, &out[..out_len]);
        TTY_OUTPUT_INFLIGHT[slot].fetch_sub(1, Ordering::Release);
        TTY_OUTPUT_WAITERS[slot].wake_all();
    }

    // IXOFF — send XOFF byte to terminal if high-water exceeded.
    if let Some((driver_id, xoff)) = ixoff_byte_out {
        write_driver_unlocked(driver_id, &[xoff]);
    }

    if let Some((pgid, sig)) = deferred_signal {
        if pgid != 0 {
            let _ = signal_process_group(pgid, sig);
        }
    }

    if wake {
        notify_input_ready(idx);
    }

    // Wake blocked writers and poll waiters on IXON resume.
    if output_resumed {
        TTY_OUTPUT_WAITERS[slot].wake_all();
        TTY_POLL_WAITERS[slot].wake_all();
    }
}

/// Wake one task blocked on input for a specific TTY.
fn notify_input_ready(idx: TtyIndex) {
    if scheduler_is_enabled() == 0 {
        return;
    }
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    TTY_INPUT_WAITERS[slot].wake_one();
    // Wake per-slot poll sleepers.
    TTY_POLL_WAITERS[slot].wake_all();
}

// ---------------------------------------------------------------------------
// Read path
// ---------------------------------------------------------------------------

/// Read cooked data from a specific TTY.
///
/// Uses `TtySession::check_read()` as the sole read-side gate.  Background
/// processes receive `SIGTTIN` instead of silently blocking.
///
/// drain + foreground check + read are merged into a single per-TTY
/// lock acquisition per loop iteration (previously 5–6 separate locks).
#[must_use]
pub fn read(idx: TtyIndex, buf: &mut [u8], nonblock: bool) -> Result<usize, TtyError> {
    read_with_attach(idx, buf, nonblock, true)
}

/// Note: `_auto_attach` is intentionally dead.  Durable read-side ownership
/// mutation has been removed, so reads no longer claim controlling
/// terminal regardless of this flag.  The parameter is preserved to maintain
/// ABI compatibility with the kernel services trait (`read_cooked_with_attach`).

pub fn read_with_attach(
    idx: TtyIndex,
    buf: &mut [u8],
    nonblock: bool,
    _auto_attach: bool,
) -> Result<usize, TtyError> {
    if buf.is_empty() {
        return Ok(0);
    }
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    register_idle_callback();
    let task_id = current_task_id();
    let caller_pgid = current_task_pgid();
    let caller_sid = current_task_sid();
    let enforce_access = task_id != 0;

    let mut total = 0usize;

    loop {
        // --- Single lock acquisition: state check + drain + foreground + read ---
        let deferred_signal;
        let mut should_wait = false;
        let mut wait_timeout_ms: Option<u64> = None;
        let mut ixoff_xon = None;
        // Track if this read unthrottled the TTY so we
        // can wake the master-side writer after releasing the lock.
        let mut unthrottled_peer: Option<usize> = None;
        // Track if no_room recovery happened so we
        // can wake producers after releasing the lock.
        let mut no_room_recovered = false;
        {
            let mut guard = TTY_SLOTS[slot].lock();
            let tty = match guard.as_mut() {
                Some(t) => t,
                None => return Err(TtyError::NotAllocated),
            };

            // Post-hangup I/O hardening — a hung-up TTY is
            // permanently dead.  Reads always return EOF (0 bytes) once
            // any buffered data has been consumed, regardless of whether
            // the read is blocking or non-blocking.
            if tty.peer_closed && !tty.ldisc.has_data() {
                return Ok(0);
            }

            if tty.hung_up && !tty.ldisc.has_data() {
                return Ok(0);
            }

            // Foreground check via check_read().
            if enforce_access {
                match tty.session.check_read(caller_pgid, caller_sid) {
                    ForegroundCheck::BackgroundRead => {
                        drop(guard);
                        if is_current_signal_blocked_or_ignored(SIGTTIN)
                            || is_pgrp_orphaned(caller_pgid, caller_sid)
                        {
                            return Err(TtyError::HungUp);
                        }
                        if caller_pgid != 0 {
                            let _ = signal_process_group(caller_pgid, SIGTTIN);
                        }
                        return Err(TtyError::BackgroundRead);
                    }
                    ForegroundCheck::DeniedCrossSession => {
                        return Err(TtyError::CrossSessionDenied);
                    }
                    ForegroundCheck::Allowed | ForegroundCheck::BootstrapAllowed => {}
                    ForegroundCheck::BackgroundWrite => {
                        // Should not happen on read path, treat as allowed.
                    }
                }
            }

            // Drain hardware input (merged — single lock for drain + read).
            deferred_signal = tty.drain_hw_input_locked();

            // PTY packet mode — if this master has pending
            // packet events, return a single-byte read with the event
            // bitmask (consuming the events).  If no events but data is
            // available, prefix the read with TIOCPKT_DATA (0x00).
            if tty.packet_mode && total == 0 {
                if tty.packet_events != 0 {
                    buf[0] = tty.packet_events;
                    tty.packet_events = 0;
                    drop(guard);
                    if let Some((pgid, sig)) = deferred_signal {
                        if pgid != 0 {
                            let _ = signal_process_group(pgid, sig);
                        }
                    }
                    return Ok(1);
                }
                // Reserve buf[0] for the TIOCPKT_DATA prefix byte.
                if buf.len() < 2 {
                    // Not enough room for prefix + data; fall through
                    // to the wait logic below.
                } else {
                    let got = tty.ldisc.read(&mut buf[1..]);
                    if got > 0 {
                        buf[0] = slopos_abi::syscall::TIOCPKT_DATA;
                        total = 1 + got;
                        ixoff_xon = tty
                            .ldisc
                            .ixoff_check_xon()
                            .map(|xon| (tty.driver.id(), xon));
                        // Unthrottle after packet-mode read.
                        let mut pkt_unthrottled_peer = if tty.throttled
                            && tty.ldisc.bytes_available() <= ldisc::THROTTLE_LOW_WATER
                        {
                            tty.throttled = false;
                            match &tty.driver {
                                TtyDriverKind::PtySlave { peer } => Some(peer.idx.0 as usize),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        // No-room recovery after packet-mode drain.
                        let pkt_no_room_recovered = tty.ldisc.check_no_room_recovery();
                        if pkt_no_room_recovered && pkt_unthrottled_peer.is_none() {
                            if let TtyDriverKind::PtySlave { peer } = &tty.driver {
                                pkt_unthrottled_peer = Some(peer.idx.0 as usize);
                            }
                        }
                        // Canonical mode: return immediately with prefix.
                        drop(guard);
                        if let Some((driver_id, xon)) = ixoff_xon {
                            write_driver_unlocked(driver_id, &[xon]);
                        }
                        if let Some((pgid, sig)) = deferred_signal {
                            if pgid != 0 {
                                let _ = signal_process_group(pgid, sig);
                            }
                        }
                        // Wake master-side writer after unthrottle.
                        if let Some(ps) = pkt_unthrottled_peer {
                            if ps < MAX_TTYS {
                                TTY_OUTPUT_WAITERS[ps].wake_all();
                                TTY_POLL_WAITERS[ps].wake_all();
                            }
                        }
                        // Wake local waiters on no_room recovery.
                        if pkt_no_room_recovered {
                            TTY_INPUT_WAITERS[slot].wake_all();
                            TTY_POLL_WAITERS[slot].wake_all();
                        }
                        return Ok(total);
                    }
                    // No data available — fall through to wait logic.
                }
            } else {
                // Normal (non-packet) read path.
                let got = tty.ldisc.read(&mut buf[total..]);
                total = total.saturating_add(got);
                if got > 0 {
                    ixoff_xon = tty
                        .ldisc
                        .ixoff_check_xon()
                        .map(|xon| (tty.driver.id(), xon));
                }
                // Unthrottle if occupancy dropped below low-water.
                if got > 0
                    && tty.throttled
                    && tty.ldisc.bytes_available() <= ldisc::THROTTLE_LOW_WATER
                {
                    tty.throttled = false;
                    if let TtyDriverKind::PtySlave { peer } = &tty.driver {
                        unthrottled_peer = Some(peer.idx.0 as usize);
                    }
                }
                // No-room recovery after normal drain.
                if got > 0 && tty.ldisc.check_no_room_recovery() {
                    no_room_recovered = true;
                    if unthrottled_peer.is_none() {
                        if let TtyDriverKind::PtySlave { peer } = &tty.driver {
                            unthrottled_peer = Some(peer.idx.0 as usize);
                        }
                    }
                }
            }

            let is_canonical = tty.ldisc.is_canonical();
            let (vmin_u8, vtime_u8) = tty.ldisc.vmin_vtime();
            let vmin = core::cmp::min(vmin_u8 as usize, buf.len());
            let vtime_ms = (vtime_u8 as u64) * 100;

            if is_canonical {
                if total > 0 {
                    // Drop guard before delivering deferred signal.
                    drop(guard);
                    if let Some((driver_id, xon)) = ixoff_xon {
                        write_driver_unlocked(driver_id, &[xon]);
                    }
                    if let Some((pgid, sig)) = deferred_signal {
                        if pgid != 0 {
                            let _ = signal_process_group(pgid, sig);
                        }
                    }
                    // Wake master after unthrottle.
                    if let Some(ps) = unthrottled_peer {
                        if ps < MAX_TTYS {
                            TTY_OUTPUT_WAITERS[ps].wake_all();
                            TTY_POLL_WAITERS[ps].wake_all();
                        }
                    }
                    // Wake local waiters on no_room recovery.
                    if no_room_recovered {
                        TTY_INPUT_WAITERS[slot].wake_all();
                        TTY_POLL_WAITERS[slot].wake_all();
                    }
                    return Ok(total);
                }
            } else {
                match (vmin_u8, vtime_u8) {
                    (0, 0) => {
                        drop(guard);
                        if let Some((driver_id, xon)) = ixoff_xon {
                            write_driver_unlocked(driver_id, &[xon]);
                        }
                        if let Some((pgid, sig)) = deferred_signal {
                            if pgid != 0 {
                                let _ = signal_process_group(pgid, sig);
                            }
                        }
                        return Ok(total);
                    }
                    (0, _) => {
                        if total > 0 {
                            drop(guard);
                            if let Some((driver_id, xon)) = ixoff_xon {
                                write_driver_unlocked(driver_id, &[xon]);
                            }
                            if let Some((pgid, sig)) = deferred_signal {
                                if pgid != 0 {
                                    let _ = signal_process_group(pgid, sig);
                                }
                            }
                            return Ok(total);
                        }
                        should_wait = true;
                        wait_timeout_ms = Some(vtime_ms);
                    }
                    (_, 0) => {
                        if total >= vmin {
                            drop(guard);
                            if let Some((driver_id, xon)) = ixoff_xon {
                                write_driver_unlocked(driver_id, &[xon]);
                            }
                            if let Some((pgid, sig)) = deferred_signal {
                                if pgid != 0 {
                                    let _ = signal_process_group(pgid, sig);
                                }
                            }
                            return Ok(total);
                        }
                        should_wait = true;
                    }
                    (_, _) => {
                        // POSIX VMIN>0 / VTIME>0: inter-byte timeout.
                        // The timer starts after the first byte arrives,
                        // NOT when read() is called.
                        if total >= vmin {
                            drop(guard);
                            if let Some((driver_id, xon)) = ixoff_xon {
                                write_driver_unlocked(driver_id, &[xon]);
                            }
                            if let Some((pgid, sig)) = deferred_signal {
                                if pgid != 0 {
                                    let _ = signal_process_group(pgid, sig);
                                }
                            }
                            return Ok(total);
                        }
                        should_wait = true;
                        // no bytes yet — wait indefinitely for
                        // the first byte (timeout = None).
                        // at least one byte received — start the
                        // inter-byte timer for the remaining bytes.
                        if total > 0 {
                            wait_timeout_ms = Some(vtime_ms);
                        }
                        // else: wait_timeout_ms remains None (indefinite)
                    }
                }
            }

            // Check hung-up after drain (data may have been
            // flushed by hangup).  Always EOF regardless of blocking mode.
            if tty.peer_closed && !tty.ldisc.has_data() {
                return Ok(0);
            }

            if tty.hung_up && !tty.ldisc.has_data() {
                return Ok(0);
            }

            if !is_canonical && !should_wait {
                if total > 0 {
                    drop(guard);
                    if let Some((driver_id, xon)) = ixoff_xon {
                        write_driver_unlocked(driver_id, &[xon]);
                    }
                    if let Some((pgid, sig)) = deferred_signal {
                        if pgid != 0 {
                            let _ = signal_process_group(pgid, sig);
                        }
                    }
                    return Ok(total);
                }
            }
        }
        // --- Per-TTY lock dropped ---

        if let Some((driver_id, xon)) = ixoff_xon {
            write_driver_unlocked(driver_id, &[xon]);
        }

        // Deliver deferred signal from drain (e.g. Ctrl+C on serial).
        if let Some((pgid, sig)) = deferred_signal {
            if pgid != 0 {
                let _ = signal_process_group(pgid, sig);
            }
        }

        // Wake master-side writer if this read unthrottled.
        if let Some(ps) = unthrottled_peer {
            if ps < MAX_TTYS {
                TTY_OUTPUT_WAITERS[ps].wake_all();
                TTY_POLL_WAITERS[ps].wake_all();
            }
        }

        // Wake local waiters on no_room recovery.
        if no_room_recovered {
            TTY_INPUT_WAITERS[slot].wake_all();
            TTY_POLL_WAITERS[slot].wake_all();
        }

        if nonblock {
            return if total > 0 {
                Ok(total)
            } else {
                Err(TtyError::WouldBlock)
            };
        }

        // Block on per-TTY wait queue.
        let wait_condition = || {
            // Check for pending signals so the wait
            // breaks out and the read can return ERESTARTSYS.
            if has_pending_signal() {
                return true;
            }
            let (sig, result) = {
                let mut guard = TTY_SLOTS[slot].lock();
                match guard.as_mut() {
                    Some(tty) => {
                        if enforce_access {
                            if matches!(
                                tty.session.check_read(caller_pgid, caller_sid),
                                ForegroundCheck::BackgroundRead
                                    | ForegroundCheck::DeniedCrossSession
                            ) {
                                return false;
                            }
                        }
                        let sig = tty.drain_hw_input_locked();
                        let result = tty.hung_up || tty.peer_closed || tty.ldisc.has_data();
                        (sig, result)
                    }
                    None => return true,
                }
            };
            // Deliver deferred signal outside lock.
            if let Some((pgid, signum)) = sig {
                if pgid != 0 {
                    let _ = signal_process_group(pgid, signum);
                }
            }
            result
        };

        let wait_ok = match wait_timeout_ms {
            Some(timeout_ms) => {
                TTY_INPUT_WAITERS[slot].wait_event_timeout(wait_condition, timeout_ms)
            }
            None => TTY_INPUT_WAITERS[slot].wait_event(wait_condition),
        };
        if !wait_ok {
            return if total > 0 { Ok(total) } else { Ok(0) };
        }

        // If the wait was broken by a pending signal
        // rather than data arrival, return partial data (if any) or
        // ERESTARTSYS so the syscall return path can handle SA_RESTART.
        if has_pending_signal() {
            return if total > 0 {
                Ok(total)
            } else {
                Err(TtyError::Restart)
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Write path
// ---------------------------------------------------------------------------

/// Write bytes to a specific TTY.
///
/// Applies output processing (`c_oflag`) — e.g. OPOST + ONLCR converts
/// `\n` to `\r\n` before sending to the driver.
///
/// split-write pattern — output is processed through the line
/// discipline under the per-TTY lock into a local stack buffer, the lock is
/// dropped, and the buffered bytes are written to the hardware without
/// holding any TTY lock.  This prevents slow serial I/O from blocking
/// operations on other TTYs.
///
/// write-side foreground check — when `TOSTOP` is set in the
/// TTY's `c_lflag`, background processes receive `SIGTTOU` instead of
/// being silently allowed to write.  This matches POSIX job control.
///
/// TOSTOP audit — added SIGTTOU blocked/ignored bypass and
/// orphaned process group → EIO handling to match `tcsetattr` semantics.
#[must_use]
pub fn write(idx: TtyIndex, data: &[u8], nonblock: bool) -> Result<usize, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Post-hangup I/O hardening — writes to a hung-up TTY
    // always return EIO.  The data has nowhere to go.
    {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.hung_up {
                return Err(TtyError::HungUp);
            }
        }
    }

    // Write-side foreground check.
    // Enforce cross-session denial and TOSTOP.
    // bypass if SIGTTOU is blocked/ignored; return EIO for
    // orphaned background process groups.
    // Only enforce for real tasks (task_id != 0 avoids early-boot writes).
    let task_id = current_task_id();
    if task_id != 0 {
        let caller_pgid = current_task_pgid();
        let caller_sid = current_task_sid();
        let guard = TTY_SLOTS[slot].lock();
        let check_result = match guard.as_ref() {
            Some(tty) => {
                let tostop = tty
                    .ldisc
                    .termios()
                    .local_flags()
                    .contains(LocalFlags::TOSTOP);
                Some(tty.session.check_write(caller_pgid, caller_sid, tostop))
            }
            None => return Err(TtyError::NotAllocated),
        };
        drop(guard);

        match check_result {
            Some(ForegroundCheck::BackgroundWrite) => {
                // if SIGTTOU is blocked or ignored, proceed silently.
                if !is_current_signal_blocked_or_ignored(SIGTTOU) {
                    // orphaned pgrp → EIO.
                    if is_pgrp_orphaned(caller_pgid, caller_sid) {
                        return Err(TtyError::OrphanedProcessGroup);
                    }
                    if caller_pgid != 0 {
                        let _ = signal_process_group(caller_pgid, SIGTTOU);
                    }
                    return Err(TtyError::BackgroundWrite);
                }
                // SIGTTOU blocked or ignored — fall through to write.
            }
            Some(ForegroundCheck::DeniedCrossSession) => {
                return Err(TtyError::CrossSessionDenied);
            }
            _ => {}
        }
    }

    // Maximum output bytes per chunk.  Each input byte can expand to at most
    // 2 output bytes (e.g. NL → CR+NL with ONLCR).  256 bytes leaves room
    // for expansion while keeping the stack buffer small.
    const OUT_BUF_CAP: usize = 256;

    // Determine if this TTY is a PTY master so we can
    // apply slave-side throttle back-pressure in the write loop.
    let peer_slave_slot: Option<usize> = {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(tty) => match &tty.driver {
                TtyDriverKind::PtyMaster { peer } => Some(peer.idx.0 as usize),
                _ => None,
            },
            None => None,
        }
    };
    let peer_master_slot: Option<usize> = {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(tty) => match &tty.driver {
                TtyDriverKind::PtySlave { peer } => Some(peer.idx.0 as usize),
                _ => None,
            },
            None => None,
        }
    };
    let mut pos = 0;
    while pos < data.len() {
        // PTY master throttle back-pressure.
        // If the peer slave is throttled, block until the slave reader
        // drains enough data to unthrottle.  Non-blocking writes return
        // a short write (or EAGAIN if no bytes written yet) instead of
        // blocking.
        if let Some(peer_slot) = peer_slave_slot {
            if nonblock {
                let guard = TTY_SLOTS[peer_slot].lock();
                let is_throttled = match guard.as_ref() {
                    Some(tty) => tty.throttled && !tty.hung_up && !tty.peer_closed,
                    None => false,
                };
                drop(guard);
                if is_throttled {
                    return if pos > 0 {
                        Ok(pos) // short write
                    } else {
                        Err(TtyError::WouldBlock)
                    };
                }
            } else {
                TTY_OUTPUT_WAITERS[peer_slot].wait_event(|| {
                    if has_pending_signal() {
                        return true;
                    }
                    let guard = TTY_SLOTS[peer_slot].lock();
                    match guard.as_ref() {
                        Some(tty) => !tty.throttled || tty.hung_up || tty.peer_closed,
                        None => true,
                    }
                });
                if has_pending_signal() {
                    return if pos > 0 {
                        Ok(pos)
                    } else {
                        Err(TtyError::Restart)
                    };
                }
            }
            // Re-check hangup after unblock.
            {
                let guard = TTY_SLOTS[slot].lock();
                if let Some(tty) = guard.as_ref() {
                    if tty.hung_up {
                        return if pos > 0 {
                            Ok(pos)
                        } else {
                            Err(TtyError::HungUp)
                        };
                    }
                }
            }
        }

        if let Some(master_slot) = peer_master_slot {
            if nonblock {
                let guard = TTY_SLOTS[master_slot].lock();
                let is_full = match guard.as_ref() {
                    Some(tty) => tty.ldisc.input_full(),
                    None => false,
                };
                drop(guard);
                if is_full {
                    return if pos > 0 {
                        Ok(pos)
                    } else {
                        Err(TtyError::WouldBlock)
                    };
                }
            } else {
                TTY_OUTPUT_WAITERS[master_slot].wait_event(|| {
                    let guard = TTY_SLOTS[master_slot].lock();
                    match guard.as_ref() {
                        Some(tty) => !tty.ldisc.input_full() || tty.hung_up || tty.peer_closed,
                        None => true,
                    }
                });
            }
            {
                let guard = TTY_SLOTS[slot].lock();
                if let Some(tty) = guard.as_ref() {
                    if tty.hung_up {
                        return if pos > 0 {
                            Ok(pos)
                        } else {
                            Err(TtyError::HungUp)
                        };
                    }
                }
            }
        }

        // Output-stop enforcement.
        // Block the writer when EITHER the line discipline is stopped
        // (IXON: Ctrl+S / VSTOP) OR the TTY has been explicitly stopped
        // via tcxonc(TCOOFF).  Non-blocking writes return a short write
        // (or EAGAIN if no bytes written yet) instead of blocking.
        if nonblock {
            let guard = TTY_SLOTS[slot].lock();
            let is_stopped = match guard.as_ref() {
                Some(tty) => tty.ldisc.is_stopped() || tty.output_stopped,
                None => false,
            };
            drop(guard);
            if is_stopped {
                return if pos > 0 {
                    Ok(pos) // short write
                } else {
                    Err(TtyError::WouldBlock)
                };
            }
        } else {
            TTY_OUTPUT_WAITERS[slot].wait_event(|| {
                if has_pending_signal() {
                    return true;
                }
                let guard = TTY_SLOTS[slot].lock();
                match guard.as_ref() {
                    Some(tty) => !tty.ldisc.is_stopped() && !tty.output_stopped,
                    None => true, // slot gone — let the next lock attempt return NotAllocated
                }
            });
            if has_pending_signal() {
                return if pos > 0 {
                    Ok(pos)
                } else {
                    Err(TtyError::Restart)
                };
            }
        }

        let mut out_buf = [0u8; OUT_BUF_CAP];
        let mut out_len = 0;
        let driver_id;

        // Process output under per-TTY lock (fast — pure computation).
        {
            let mut guard = TTY_SLOTS[slot].lock();
            let tty = match guard.as_mut() {
                Some(t) => t,
                None => return Err(TtyError::NotAllocated),
            };
            driver_id = tty.driver.id();

            while pos < data.len() {
                match tty.ldisc.process_output_byte(data[pos]) {
                    OutputAction::Emit { buf, len } => {
                        for i in 0..len as usize {
                            if out_len < OUT_BUF_CAP {
                                out_buf[out_len] = buf[i];
                                out_len += 1;
                            }
                        }
                    }
                    OutputAction::Tab(n) => {
                        for _ in 0..n as usize {
                            if out_len < OUT_BUF_CAP {
                                out_buf[out_len] = b' ';
                                out_len += 1;
                            }
                        }
                    }
                    OutputAction::Suppress => {}
                }
                pos += 1;
                // If buffer nearly full, break to flush.
                if out_len >= OUT_BUF_CAP - 8 {
                    break;
                }
            }
        }
        // Per-TTY lock dropped.

        // Driver I/O without any TTY lock (slow — hardware).
        // Track in-flight output for drain semantics.
        TTY_OUTPUT_INFLIGHT[slot].fetch_add(1, Ordering::Release);
        let driver_written = write_driver_unlocked(driver_id, &out_buf[..out_len]);
        TTY_OUTPUT_INFLIGHT[slot].fetch_sub(1, Ordering::Release);
        if driver_written < out_len {
            break;
        }
        // Wake drain waiters (TCSETSW / TCSETSF) now that this chunk
        // has reached the hardware.
        TTY_OUTPUT_WAITERS[slot].wake_all();
    }

    Ok(pos)
}

// ---------------------------------------------------------------------------
// Data availability queries
// ---------------------------------------------------------------------------

/// Check if a TTY has cooked data available for reading.
///
/// Properly captures and delivers deferred signals from
/// `drain_hw_input_locked()` instead of silently discarding them.
pub fn has_data(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    let (deferred_signal, result) = {
        let mut guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_mut() {
            let sig = tty.drain_hw_input_locked();
            let data = tty.ldisc.has_data();
            (sig, data)
        } else {
            return false;
        }
    };
    // Deliver deferred signal outside lock to avoid deadlock.
    if let Some((pgid, sig)) = deferred_signal {
        if pgid != 0 {
            let _ = signal_process_group(pgid, sig);
        }
    }
    result
}

/// Get the number of bytes available for reading from a TTY.
///
/// Used by the FIONREAD / TIOCINQ ioctl.  Drains pending hardware input
/// first to ensure the count is up-to-date.
#[must_use]
pub fn bytes_available(idx: TtyIndex) -> Result<usize, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let (deferred_signal, count) = {
        let mut guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_mut() {
            let sig = tty.drain_hw_input_locked();
            let n = tty.ldisc.bytes_available();
            (sig, n)
        } else {
            return Err(TtyError::NotAllocated);
        }
    };
    if let Some((pgid, sig)) = deferred_signal {
        if pgid != 0 {
            let _ = signal_process_group(pgid, sig);
        }
    }
    Ok(count)
}

/// Get the number of bytes queued for output on a TTY.
///
/// Used by the `TIOCOUTQ` ioctl.  Returns the sum of:
///   1. The per-TTY inflight counter (`TTY_OUTPUT_INFLIGHT`) — bytes that
///      have been processed by the line discipline but not yet transmitted
///      to the hardware driver.
///   2. Driver-level pending output (for async/interrupt-driven drivers).
///
/// For synchronous backends (serial console, vconsole) the driver pending
/// count is always zero because `write_output` blocks until the byte is on
/// the wire.  For PTYs, output is immediately buffered in the peer so the
/// driver also reports zero.
#[must_use]
pub fn output_queued_bytes(idx: TtyIndex) -> Result<usize, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let inflight = TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire) as usize;
    let driver_pending = {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            // use output_pending_bytes() for finer-grained
            // queue depth reporting (defaults to 0/1 for bool-only drivers).
            tty.driver.output_pending_bytes()
        } else {
            return Err(TtyError::NotAllocated);
        }
    };
    Ok(inflight + driver_pending)
}

// ---------------------------------------------------------------------------
// Idle callback
// ---------------------------------------------------------------------------

/// Idle-loop callback: drain hardware input and wake blocked readers.
///
/// now iterates all active TTYs instead of only TTY 0.  Each
/// per-TTY lock is acquired and released individually.
///
/// Properly captures and delivers deferred signals from
/// `drain_hw_input_locked()` instead of silently discarding them.
fn input_available_cb() -> c_int {
    let mut any_data = false;
    for i in 0..MAX_TTYS {
        let (deferred_signal, has_data) = {
            let mut guard = TTY_SLOTS[i].lock();
            if let Some(tty) = guard.as_mut() {
                if tty.active {
                    let sig = tty.drain_hw_input_locked();
                    let data = tty.ldisc.has_data();
                    (sig, data)
                } else {
                    (None, false)
                }
            } else {
                (None, false)
            }
        };
        // Deliver deferred signal outside lock.
        if let Some((pgid, sig)) = deferred_signal {
            if pgid != 0 {
                let _ = signal_process_group(pgid, sig);
            }
        }
        if has_data {
            notify_input_ready(TtyIndex(i as u8));
            any_data = true;
        }
    }
    any_data as c_int
}

pub(super) fn register_idle_callback() {
    use core::sync::atomic::{AtomicBool, Ordering};
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    register_idle_wakeup_callback(Some(input_available_cb));
}
