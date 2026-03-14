//! TTY I/O paths — read, write, push_input, hardware drain, data queries,
//! and the idle-loop input callback.
//!
//! decomposition: extracted from `mod.rs` to isolate the hot data
//! paths (read/write/push_input) from termios configuration, lifecycle
//! management, job control, and poll readiness.
//!
//! # Echo serialisation
//!
//! Unlike Linux, which uses a separate echo buffer + deferred processing,
//! SlopOS accumulates echo bytes into `EchoBuf` during `receive_buf()` and
//! writes them atomically under `TTY_WRITE_LOCKS[slot]`.  User writes also
//! acquire the same per-slot write lock, so echo and user output never
//! interleave at the byte level.  A separate echo buffer is therefore not
//! needed for correctness — the write lock provides the POSIX §11.1.9
//! serialisation guarantee.

use core::ffi::c_int;
use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIGTTIN, SIGTTOU};
use slopos_abi::syscall::LocalFlags;

use slopos_lib::kernel_services::driver_runtime::{
    current_task_id, current_task_pgid, current_task_sid, has_pending_signal,
    is_current_signal_blocked_or_ignored, is_pgrp_orphaned, register_idle_wakeup_callback,
    scheduler_is_enabled, signal_process_group,
};

use super::driver::{InputEvent, TtyDriverKind, write_driver_unlocked};
use super::ldisc::{self, BatchResult, OutputAction};
use super::session::ForegroundCheck;
use super::table::{
    TTY_INPUT_WAITERS, TTY_OUTPUT_INFLIGHT, TTY_OUTPUT_WAITERS, TTY_POLL_WAITERS, TTY_SLOTS,
    TTY_WRITE_LOCKS,
};
use super::{MAX_TTYS, PacketEvents, PostLockWork, Tty, TtyError, TtyFlags, TtyIndex};

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
        // Feed raw hardware bytes directly to the line discipline.
        // The ldisc handles all input mapping via c_iflag processing:
        //   - CR→NL: handled by ICRNL in process_iflag()
        //   - NL→CR: handled by INLCR in process_iflag()
        //   - DEL (0x7F): matched against VERASE (default 0x7F) in canonical_input()
        // Pre-mapping here would bypass POSIX input flag semantics (e.g.
        // IGNCR could not discard CR if it was already mapped to NL).
        for i in 0..count {
            events[i] = InputEvent::normal(scratch[i]);
        }

        let batch = self.ldisc.receive_buf(&events[..count]);
        let xoff = self.ldisc.ixoff_check_xoff();
        if !batch.echo.is_empty() || xoff.is_some() {
            let slot = self.index.0 as usize;
            let _write_guard = TTY_WRITE_LOCKS[slot].lock();
            if !batch.echo.is_empty() {
                self.driver.write_output(batch.echo.as_slice());
            }
            if let Some(xoff_byte) = xoff {
                self.driver.write_output(&[xoff_byte]);
            }
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
    pty_open_peer, pty_open_slave, queue_packet_event, set_packet_mode, set_pty_lock,
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

    let mut deferred = PostLockWork::new();
    let mut route: Option<(super::driver::DriverId, [u8; 256], usize)> = None;
    let wake = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(t) => t,
            None => return,
        };

        if tty.flags.contains(TtyFlags::HUNG_UP) {
            return;
        }

        let was_stopped = tty.ldisc.is_stopped();

        let batch: BatchResult = tty.ldisc.receive_buf(events);
        if let Some(xoff) = tty.ldisc.ixoff_check_xoff() {
            deferred.add_ixoff_byte(tty.driver.id(), xoff, slot);
        }

        if was_stopped && !tty.ldisc.is_stopped() {
            deferred.wake_output_and_poll(slot);
        }

        if !was_stopped && tty.ldisc.is_stopped() {
            deferred.add_packet_event(idx, slopos_abi::syscall::TIOCPKT_STOP);
        } else if was_stopped && !tty.ldisc.is_stopped() {
            deferred.add_packet_event(idx, slopos_abi::syscall::TIOCPKT_START);
        }

        if batch.throttle_check
            && !tty.flags.contains(TtyFlags::THROTTLED)
            && tty.ldisc.bytes_available() >= ldisc::THROTTLE_HIGH_WATER
        {
            tty.flags.insert(TtyFlags::THROTTLED);
        }

        let echo_len = batch.echo.len();
        if echo_len > 0 {
            let mut out = [0u8; 256];
            out[..echo_len].copy_from_slice(batch.echo.as_slice());
            route = Some((tty.driver.id(), out, echo_len));
        }

        if let Some((sig, _)) = batch.signal {
            deferred.add_signal(tty.session.fg_pgrp_raw(), sig);
        }

        batch.should_wake
    };

    if let Some((driver_id, out, out_len)) = route {
        let _write_guard = TTY_WRITE_LOCKS[slot].lock();
        TTY_OUTPUT_INFLIGHT[slot].fetch_add(out_len as u32, Ordering::Release);
        write_driver_unlocked(driver_id, &out[..out_len]);
        TTY_OUTPUT_INFLIGHT[slot].fetch_sub(out_len as u32, Ordering::Release);
        drop(_write_guard);
        TTY_OUTPUT_WAITERS[slot].wake_all();
    }

    if wake {
        notify_input_ready(idx);
    }

    deferred.execute();
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

fn check_read_foreground(tty: &Tty, caller_pgid: u32, caller_sid: u32) -> Result<(), TtyError> {
    match tty.session.check_read(caller_pgid, caller_sid) {
        ForegroundCheck::BackgroundRead => Err(TtyError::BackgroundRead),
        ForegroundCheck::DeniedCrossSession => Err(TtyError::CrossSessionDenied),
        ForegroundCheck::Allowed
        | ForegroundCheck::BootstrapAllowed
        | ForegroundCheck::BackgroundWrite => Ok(()),
    }
}

fn drain_and_recover(tty: &mut Tty, slot: usize, deferred: &mut PostLockWork) -> bool {
    let mut woke_peers = false;

    if tty.flags.contains(TtyFlags::THROTTLED)
        && tty.ldisc.bytes_available() <= ldisc::THROTTLE_LOW_WATER
    {
        tty.flags.remove(TtyFlags::THROTTLED);
        if let TtyDriverKind::PtySlave { peer } = &tty.driver {
            deferred.wake_output_and_poll(peer.idx.0 as usize);
            woke_peers = true;
        }
    }

    if tty.ldisc.check_no_room_recovery() {
        deferred.wake_input_and_poll(slot);
        if let TtyDriverKind::PtySlave { peer } = &tty.driver {
            deferred.wake_output_and_poll(peer.idx.0 as usize);
            woke_peers = true;
        }
    }

    woke_peers
}

fn try_read_packet_mode(
    tty: &mut Tty,
    buf: &mut [u8],
    deferred: &mut PostLockWork,
) -> Option<Result<usize, TtyError>> {
    if !tty.flags.contains(TtyFlags::PACKET_MODE) {
        return None;
    }

    if !tty.packet_events.is_empty() {
        buf[0] = tty.packet_events.bits();
        tty.packet_events = PacketEvents::empty();
        return Some(Ok(1));
    }

    if buf.len() < 2 {
        if tty.ldisc.has_data() {
            return Some(Ok(0));
        }
        return None;
    }

    let got = tty.ldisc.read(&mut buf[1..]);
    if got == 0 {
        return None;
    }

    buf[0] = slopos_abi::syscall::TIOCPKT_DATA;
    let slot = tty.index.0 as usize;
    if let Some(xon) = tty.ldisc.ixoff_check_xon() {
        deferred.add_ixoff_byte(tty.driver.id(), xon, slot);
    }
    let _ = drain_and_recover(tty, slot, deferred);
    Some(Ok(1 + got))
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
        let mut deferred = PostLockWork::new();
        let mut should_wait = false;
        let mut wait_timeout_ms: Option<u64> = None;
        {
            let mut guard = TTY_SLOTS[slot].lock();
            let tty = match guard.as_mut() {
                Some(t) => t,
                None => return Err(TtyError::NotAllocated),
            };

            if tty.flags.contains(TtyFlags::PEER_CLOSED) && !tty.ldisc.has_data() {
                return Ok(0);
            }

            if tty.flags.contains(TtyFlags::HUNG_UP) && !tty.ldisc.has_data() {
                return Ok(0);
            }

            if enforce_access {
                match check_read_foreground(tty, caller_pgid, caller_sid) {
                    Err(TtyError::BackgroundRead) => {
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
                    Err(err) => return Err(err),
                    Ok(()) => {}
                }
            }

            if let Some((pgid, sig)) = tty.drain_hw_input_locked() {
                deferred.add_signal(pgid, sig);
            }

            if tty.flags.contains(TtyFlags::PACKET_MODE) && total == 0 {
                if let Some(result) = try_read_packet_mode(tty, buf, &mut deferred) {
                    drop(guard);
                    deferred.execute();
                    return result;
                }
            } else {
                let got = tty.ldisc.read(&mut buf[total..]);
                total = total.saturating_add(got);
                if got > 0 {
                    if let Some(xon) = tty.ldisc.ixoff_check_xon() {
                        deferred.add_ixoff_byte(tty.driver.id(), xon, slot);
                    }
                    let _ = drain_and_recover(tty, slot, &mut deferred);
                }
            }

            let is_canonical = tty.ldisc.is_canonical();
            let (vmin_u8, vtime_u8) = tty.ldisc.vmin_vtime();
            let vmin = core::cmp::min(vmin_u8 as usize, buf.len());
            let vtime_ms = (vtime_u8 as u64) * 100;

            if is_canonical {
                if total > 0 {
                    drop(guard);
                    deferred.execute();
                    return Ok(total);
                }
            } else {
                match (vmin_u8, vtime_u8) {
                    (0, 0) => {
                        drop(guard);
                        deferred.execute();
                        return Ok(total);
                    }
                    (0, _) => {
                        if total > 0 {
                            drop(guard);
                            deferred.execute();
                            return Ok(total);
                        }
                        should_wait = true;
                        wait_timeout_ms = Some(vtime_ms);
                    }
                    (_, 0) => {
                        if total >= vmin {
                            drop(guard);
                            deferred.execute();
                            return Ok(total);
                        }
                        should_wait = true;
                    }
                    (_, _) => {
                        if total >= vmin {
                            drop(guard);
                            deferred.execute();
                            return Ok(total);
                        }
                        should_wait = true;
                        if total > 0 {
                            wait_timeout_ms = Some(vtime_ms);
                        }
                    }
                }
            }

            if tty.flags.contains(TtyFlags::PEER_CLOSED) && !tty.ldisc.has_data() {
                return Ok(0);
            }

            if tty.flags.contains(TtyFlags::HUNG_UP) && !tty.ldisc.has_data() {
                return Ok(0);
            }

            if !is_canonical && !should_wait && total > 0 {
                drop(guard);
                deferred.execute();
                return Ok(total);
            }
        }

        deferred.execute();

        if nonblock {
            return if total > 0 {
                Ok(total)
            } else {
                Err(TtyError::WouldBlock)
            };
        }

        let wait_condition = || {
            if has_pending_signal() {
                return true;
            }
            let mut wd = PostLockWork::new();
            let result = {
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
                        if let Some((pgid, signum)) = tty.drain_hw_input_locked() {
                            wd.add_signal(pgid, signum);
                        }
                        tty.flags.contains(TtyFlags::HUNG_UP)
                            || tty.flags.contains(TtyFlags::PEER_CLOSED)
                            || tty.ldisc.has_data()
                    }
                    None => return true,
                }
            };
            wd.execute();
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

        if has_pending_signal() {
            return if total > 0 {
                Ok(total)
            } else {
                Err(TtyError::Restart)
            };
        }
    }
}

fn check_write_foreground(slot: usize) -> Result<(), TtyError> {
    let task_id = current_task_id();
    if task_id == 0 {
        return Ok(());
    }

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
            if !is_current_signal_blocked_or_ignored(SIGTTOU) {
                if is_pgrp_orphaned(caller_pgid, caller_sid) {
                    return Err(TtyError::OrphanedProcessGroup);
                }
                if caller_pgid != 0 {
                    let _ = signal_process_group(caller_pgid, SIGTTOU);
                }
                return Err(TtyError::BackgroundWrite);
            }
            Ok(())
        }
        Some(ForegroundCheck::DeniedCrossSession) => Err(TtyError::CrossSessionDenied),
        _ => Ok(()),
    }
}

fn wait_for_write_ready(
    slot: usize,
    peer_slave_slot: Option<usize>,
    peer_master_slot: Option<usize>,
    nonblock: bool,
) -> Result<(), TtyError> {
    if let Some(peer_slot) = peer_slave_slot {
        if nonblock {
            let guard = TTY_SLOTS[peer_slot].lock();
            let is_throttled = match guard.as_ref() {
                Some(tty) => {
                    tty.flags.contains(TtyFlags::THROTTLED)
                        && !tty.flags.contains(TtyFlags::HUNG_UP)
                        && !tty.flags.contains(TtyFlags::PEER_CLOSED)
                }
                None => false,
            };
            drop(guard);
            if is_throttled {
                return Err(TtyError::WouldBlock);
            }
        } else {
            TTY_OUTPUT_WAITERS[peer_slot].wait_event(|| {
                if has_pending_signal() {
                    return true;
                }
                let guard = TTY_SLOTS[peer_slot].lock();
                match guard.as_ref() {
                    Some(tty) => {
                        !tty.flags.contains(TtyFlags::THROTTLED)
                            || tty.flags.contains(TtyFlags::HUNG_UP)
                            || tty.flags.contains(TtyFlags::PEER_CLOSED)
                    }
                    None => true,
                }
            });
            if has_pending_signal() {
                return Err(TtyError::Restart);
            }
        }

        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.flags.contains(TtyFlags::HUNG_UP) {
                return Err(TtyError::HungUp);
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
                return Err(TtyError::WouldBlock);
            }
        } else {
            TTY_OUTPUT_WAITERS[master_slot].wait_event(|| {
                let guard = TTY_SLOTS[master_slot].lock();
                match guard.as_ref() {
                    Some(tty) => {
                        !tty.ldisc.input_full()
                            || tty.flags.contains(TtyFlags::HUNG_UP)
                            || tty.flags.contains(TtyFlags::PEER_CLOSED)
                    }
                    None => true,
                }
            });
        }

        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.flags.contains(TtyFlags::HUNG_UP) {
                return Err(TtyError::HungUp);
            }
        }
    }

    if nonblock {
        let guard = TTY_SLOTS[slot].lock();
        let is_stopped = match guard.as_ref() {
            Some(tty) => tty.ldisc.is_stopped() || tty.flags.contains(TtyFlags::OUTPUT_STOPPED),
            None => false,
        };
        drop(guard);
        if is_stopped {
            return Err(TtyError::WouldBlock);
        }
    } else {
        TTY_OUTPUT_WAITERS[slot].wait_event(|| {
            if has_pending_signal() {
                return true;
            }
            let guard = TTY_SLOTS[slot].lock();
            match guard.as_ref() {
                Some(tty) => {
                    !tty.ldisc.is_stopped() && !tty.flags.contains(TtyFlags::OUTPUT_STOPPED)
                }
                None => true,
            }
        });
        if has_pending_signal() {
            return Err(TtyError::Restart);
        }
    }

    Ok(())
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
            if tty.flags.contains(TtyFlags::HUNG_UP) {
                return Err(TtyError::HungUp);
            }
        }
    }

    check_write_foreground(slot)?;

    // Maximum output bytes per chunk.  Each input byte can expand to at most
    // 2 output bytes (e.g. NL → CR+NL with ONLCR).  256 bytes leaves room
    // for expansion while keeping the stack buffer small.
    const OUT_BUF_CAP: usize = 256;

    // Cache peer slot indices once — PTY peer relationships are
    // immutable for the lifetime of the pair, so a single lock acquisition
    // suffices.  Previously this required two separate locks.
    let (peer_slave_slot, peer_master_slot): (Option<usize>, Option<usize>) = {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(tty) => match &tty.driver {
                TtyDriverKind::PtyMaster { peer } => (Some(peer.idx.0 as usize), None),
                TtyDriverKind::PtySlave { peer } => (None, Some(peer.idx.0 as usize)),
                _ => (None, None),
            },
            None => (None, None),
        }
    };
    let mut pos = 0;
    while pos < data.len() {
        if let Err(err) = wait_for_write_ready(slot, peer_slave_slot, peer_master_slot, nonblock) {
            return if pos > 0 { Ok(pos) } else { Err(err) };
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
            tty.ldisc.clear_flusho();

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
        // Per-TTY lock dropped — acquire per-slot write lock to serialize
        // with concurrent echo output (POSIX §11.1.9 echo serialization).
        let driver_written = {
            let _write_guard = TTY_WRITE_LOCKS[slot].lock();
            TTY_OUTPUT_INFLIGHT[slot].fetch_add(out_len as u32, Ordering::Release);
            let written = write_driver_unlocked(driver_id, &out_buf[..out_len]);
            TTY_OUTPUT_INFLIGHT[slot].fetch_sub(out_len as u32, Ordering::Release);
            written
        };
        if driver_written < out_len {
            break;
        }
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
    let mut deferred = PostLockWork::new();
    let result = {
        let mut guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_mut() {
            if let Some((pgid, sig)) = tty.drain_hw_input_locked() {
                deferred.add_signal(pgid, sig);
            }
            tty.ldisc.has_data()
        } else {
            return false;
        }
    };
    deferred.execute();
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
    let mut deferred = PostLockWork::new();
    let count = {
        let mut guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_mut() {
            if let Some((pgid, sig)) = tty.drain_hw_input_locked() {
                deferred.add_signal(pgid, sig);
            }
            tty.ldisc.bytes_available()
        } else {
            return Err(TtyError::NotAllocated);
        }
    };
    deferred.execute();
    Ok(count)
}

/// Get the number of bytes queued for output on a TTY.
///
/// Used by the `TIOCOUTQ` ioctl.  Returns the sum of:
///   1. The per-TTY inflight byte counter (`TTY_OUTPUT_INFLIGHT`) — the
///      exact number of processed bytes currently between ldisc output
///      and hardware driver completion.
///   2. Driver-level pending output (for async/interrupt-driven drivers).
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
    let mut bits = super::table::active_slots_bitmap();
    while bits != 0 {
        let i = bits.trailing_zeros() as usize;
        bits &= bits - 1;

        let mut deferred = PostLockWork::new();
        let has_data = {
            let mut guard = TTY_SLOTS[i].lock();
            if let Some(tty) = guard.as_mut() {
                if let Some((pgid, sig)) = tty.drain_hw_input_locked() {
                    deferred.add_signal(pgid, sig);
                }
                tty.ldisc.has_data()
            } else {
                false
            }
        };
        deferred.execute();
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
