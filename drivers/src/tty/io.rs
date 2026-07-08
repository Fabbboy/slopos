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

use slopos_kernel_services::driver_runtime::{
    current_task_id, current_task_pgid, current_task_sid, has_pending_signal,
    is_current_signal_blocked_or_ignored, is_pgrp_orphaned, register_idle_wakeup_callback,
    scheduler_is_enabled, signal_process_group,
};

use super::driver::{InputEvent, TtyDriverKind, write_driver_unlocked};
use super::ldisc::{self, BatchResult, OutputAction};
use super::session::ForegroundCheck;
use super::table::{
    InflightGuard, TTY_OUTPUT_INFLIGHT, TTY_SLOTS, TTY_WRITE_LOCKS, tty_input_event,
    tty_output_event,
};
use super::{MAX_TTYS, PacketEvents, PostLockWork, Tty, TtyError, TtyFlags, TtyIndex};
use slopos_ostd::sync::BUS;

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
        // SysRq (Ctrl+T) on the serial path: runs under the per-TTY lock, so
        // only mark it pending here — `PostLockWork::execute` fires the dump
        // after every lock is dropped.
        if scratch[..count].contains(&SYSRQ_DUMP_BYTE) {
            super::sysrq_mark_pending();
        }
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
    queue_packet_event, set_packet_mode, set_pty_lock,
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

/// SysRq trigger byte: Ctrl+T. Checked on the raw input path *before* ldisc
/// processing, so the dump works even when every userland input consumer is
/// wedged (the exact situation it exists to diagnose).
const SYSRQ_DUMP_BYTE: u8 = 0x14;

pub fn push_input_batch(idx: TtyIndex, events: &[InputEvent]) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS || events.is_empty() {
        return;
    }

    if events.iter().any(|e| e.byte == SYSRQ_DUMP_BYTE) {
        // Mark only — `push_input_batch` runs in keyboard-ISR context, where
        // the dump's task-pool walk (manager lock + scratch alloc + klog
        // storm) must not run. The idle-loop input callback fires it from
        // task context within a tick.
        super::sysrq_mark_pending();
    }

    let mut deferred = PostLockWork::new();
    // Echo payload is routed via a heap-allocated 256-byte buffer so
    // the 256 B inline array never lands in this function's stack
    // frame. `KBox::zeroed()` is only invoked in the `if echo_len > 0`
    // arm below — the no-echo hot path stays allocation-free.
    let mut route: Option<(super::driver::DriverId, slopos_ostd::KBox<[u8; 256]>, usize)> = None;
    // ISIG output-flush request (NOFLSH clear): the slave's undelivered
    // output lives in the peer master's read buffer; it is discarded after
    // the slave lock drops (peer lock ordering) and before the caret echo
    // is routed, so `^C` lands in an empty buffer and is immediately
    // visible even when a flooding foreground job had filled it. The held
    // backing pins the master's slot until the flush lands.
    let mut signal_flush_master: Option<slopos_ostd::KArc<super::backing::TtyBacking>> = None;
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
            if let Ok(mut out) = slopos_ostd::KBox::<[u8; 256]>::zeroed() {
                out[..echo_len].copy_from_slice(batch.echo.as_slice());
                route = Some((tty.driver.id(), out, echo_len));
            }
            // Echo drop on alloc failure is not a correctness issue:
            // the next input tick re-echoes if the terminal is still
            // live.
        }

        if let Some((sig, flush)) = batch.signal {
            deferred.add_signal(tty.session.fg_pgrp_raw(), sig);
            // A signal char discards the foreground job's pending I/O
            // unless NOFLSH is set. The line discipline already flushed
            // its input queues; clear input throttle here as part of the
            // same flush, then flush the output side below after this lock
            // drops. The signal is posted (and its wake fires) ahead of the
            // output-event wakes in `deferred.execute()`, so a writer blocked
            // on the flushed queue observes its pending signal the moment
            // its wait predicate re-runs.
            if flush {
                if let TtyDriverKind::PtySlave { peer } = &tty.driver {
                    if let Some(master_pin) = peer.upgrade() {
                        tty.flags.remove(TtyFlags::THROTTLED);
                        signal_flush_master = Some(master_pin);
                        deferred.add_packet_event(
                            idx,
                            slopos_abi::syscall::TIOCPKT_FLUSHREAD
                                | slopos_abi::syscall::TIOCPKT_FLUSHWRITE,
                        );
                    }
                }
            }
        }

        batch.should_wake
    };

    if let Some(master_pin) = signal_flush_master {
        let master_slot = master_pin.index().0 as usize;
        {
            {
                let mut guard = TTY_SLOTS[master_slot].lock();
                if let Some(master) = guard.as_mut() {
                    master.ldisc.flush_input();
                }
            }
            // Drop the slave's pending-output accounting alongside the
            // buffer (TCOFLUSH semantics). PTY-only: synchronous backends
            // (vconsole, serial) complete output inline, so for them a
            // reset would be pure counter-corruption risk with nothing to
            // flush. `InflightGuard::drop` saturates at 0, so racing a
            // live writer's guard cannot wrap the counter.
            TTY_OUTPUT_INFLIGHT[slot].store(0, Ordering::Release);
            // Writers blocked on the full-master predicate in
            // `wait_for_write_ready` (and poll waiters) re-evaluate now
            // that the queue is empty.
            deferred.wake_output_and_poll(master_slot);
        }
    }

    if let Some((driver_id, out, out_len)) = route {
        let _write_guard = TTY_WRITE_LOCKS[slot].lock();
        let _inflight = InflightGuard::new(slot, out_len);
        write_driver_unlocked(driver_id, &out[..out_len]);
        drop(_inflight);
        drop(_write_guard);
        drop(out);
        BUS.publish(tty_output_event(slot));
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
    // Input arrival wakes readers and poll waiters alike — both park on
    // the input event queue.
    BUS.publish(tty_input_event(slot));
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

/// Surface result of a *same-session* background read, by blocking mode.
///
/// A reader that is not (yet) the foreground group is a transient
/// job-control state: the foreground handoff of a freshly spawned job, or a
/// later `tcsetpgrp`, resolves it.  A non-blocking probe (the slop-ring
/// re-probes its in-flight `OP_READ` rows through this path) therefore
/// parks as `WouldBlock` so the op stays armed and self-heals — surfacing
/// `BackgroundRead` (-EIO) would poison the async op permanently, and
/// raising SIGTTIN on every re-probe would spam the process.  A blocking
/// read keeps the POSIX surface: `BackgroundRead`, which the caller turns
/// into SIGTTIN (or `HungUp` for ignored/orphaned cases).
pub(crate) fn background_read_surface(nonblock: bool) -> TtyError {
    if nonblock {
        TtyError::WouldBlock
    } else {
        TtyError::BackgroundRead
    }
}

/// React to a read having consumed bytes: clear throttle / no-room state and
/// wake whichever peer was blocked on this TTY being full.
///
/// `master_was_full` is the PTY master's `input_full()` state captured *before*
/// the read. Reading a PTY master frees space in its `RawDisc` buffer, but a
/// slave writer (e.g. a flooding `cat`) blocked in `wait_for_write_ready`'s
/// `peer_master` arm parks on `tty_output_event(<this master slot>)` until
/// `!input_full()`. No other read-path wake covers that direction — the master
/// never enters the `THROTTLED`/`no_room` states the arms below key on — so
/// without this, large slave output stalled until an unrelated master write (a
/// keystroke) happened to publish the event. Wake those writers on the
/// full→not-full edge here.
fn drain_and_recover(
    tty: &mut Tty,
    slot: usize,
    master_was_full: bool,
    deferred: &mut PostLockWork,
) -> bool {
    let mut woke_peers = false;

    if master_was_full && matches!(tty.driver, TtyDriverKind::PtyMaster { .. }) {
        deferred.wake_output_and_poll(slot);
        woke_peers = true;
    }

    if tty.flags.contains(TtyFlags::THROTTLED)
        && tty.ldisc.bytes_available() <= ldisc::THROTTLE_LOW_WATER
    {
        tty.flags.remove(TtyFlags::THROTTLED);
        if let TtyDriverKind::PtySlave { peer } = &tty.driver {
            if let Some(master) = peer.upgrade() {
                deferred.wake_output_and_poll(master.index().0 as usize);
                woke_peers = true;
            }
        }
    }

    if tty.ldisc.check_no_room_recovery() {
        deferred.wake_input_and_poll(slot);
        if let TtyDriverKind::PtySlave { peer } = &tty.driver {
            if let Some(master) = peer.upgrade() {
                deferred.wake_output_and_poll(master.index().0 as usize);
                woke_peers = true;
            }
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

    let was_full = tty.ldisc.input_full();
    let got = tty.ldisc.read(&mut buf[1..]);
    if got == 0 {
        return None;
    }

    buf[0] = slopos_abi::syscall::TIOCPKT_DATA;
    let slot = tty.index.0 as usize;
    if let Some(xon) = tty.ldisc.ixoff_check_xon() {
        deferred.add_ixoff_byte(tty.driver.id(), xon, slot);
    }
    let _ = drain_and_recover(tty, slot, was_full, deferred);
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
                        // See `background_read_surface`: non-blocking probes
                        // park as WouldBlock (transient state, self-healing);
                        // blocking reads keep the POSIX SIGTTIN surface.
                        // Cross-session denial stays a hard error above.
                        if background_read_surface(nonblock) == TtyError::WouldBlock {
                            return Err(TtyError::WouldBlock);
                        }
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
                let was_full = tty.ldisc.input_full();
                let got = tty.ldisc.read(&mut buf[total..]);
                total = total.saturating_add(got);
                if got > 0 {
                    if let Some(xon) = tty.ldisc.ixoff_check_xon() {
                        deferred.add_ixoff_byte(tty.driver.id(), xon, slot);
                    }
                    let _ = drain_and_recover(tty, slot, was_full, &mut deferred);
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
            Some(timeout_ms) => BUS
                .subscribe(tty_input_event(slot))
                .wait_event_timeout(wait_condition, timeout_ms),
            None => BUS
                .subscribe(tty_input_event(slot))
                .wait_event(wait_condition),
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteAdmission {
    Normal,
    PriorityControlOnly,
}

fn wait_for_write_ready(
    slot: usize,
    peer_slave_slot: Option<usize>,
    peer_master_slot: Option<usize>,
    nonblock: bool,
    next_priority_byte: Option<u8>,
) -> Result<WriteAdmission, TtyError> {
    let mut admission = WriteAdmission::Normal;

    if let Some(peer_slot) = peer_slave_slot {
        if nonblock {
            let guard = TTY_SLOTS[peer_slot].lock();
            let blocked_by_throttle = match guard.as_ref() {
                Some(tty) => {
                    let throttled = tty.flags.contains(TtyFlags::THROTTLED)
                        && !tty.flags.contains(TtyFlags::HUNG_UP)
                        && !tty.flags.contains(TtyFlags::PEER_CLOSED);
                    if throttled {
                        let priority = next_priority_byte
                            .map(|byte| tty.ldisc.priority_control_input(byte))
                            .unwrap_or(false);
                        if priority {
                            admission = WriteAdmission::PriorityControlOnly;
                            false
                        } else {
                            true
                        }
                    } else {
                        false
                    }
                }
                None => false,
            };
            drop(guard);
            if blocked_by_throttle {
                return Err(TtyError::WouldBlock);
            }
        } else {
            BUS.subscribe(tty_output_event(peer_slot)).wait_event(|| {
                if has_pending_signal() {
                    return true;
                }
                let guard = TTY_SLOTS[peer_slot].lock();
                match guard.as_ref() {
                    Some(tty) => {
                        !tty.flags.contains(TtyFlags::THROTTLED)
                            || tty.flags.contains(TtyFlags::HUNG_UP)
                            || tty.flags.contains(TtyFlags::PEER_CLOSED)
                            || next_priority_byte
                                .map(|byte| tty.ldisc.priority_control_input(byte))
                                .unwrap_or(false)
                    }
                    None => true,
                }
            });
            if has_pending_signal() {
                return Err(TtyError::Restart);
            }

            let guard = TTY_SLOTS[peer_slot].lock();
            if let Some(tty) = guard.as_ref() {
                if tty.flags.contains(TtyFlags::THROTTLED)
                    && !tty.flags.contains(TtyFlags::HUNG_UP)
                    && !tty.flags.contains(TtyFlags::PEER_CLOSED)
                    && next_priority_byte
                        .map(|byte| tty.ldisc.priority_control_input(byte))
                        .unwrap_or(false)
                {
                    admission = WriteAdmission::PriorityControlOnly;
                }
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
            // Signal-interruptible like the slave arm above: a foreground
            // job blocked here while flooding a full master MUST unwind on
            // Ctrl-C — the wait predicate alone can stay false forever if
            // the master side stops draining, and delivery only happens at
            // syscall exit, which this wait would otherwise never reach.
            BUS.subscribe(tty_output_event(master_slot)).wait_event(|| {
                if has_pending_signal() {
                    return true;
                }
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
        BUS.subscribe(tty_output_event(slot)).wait_event(|| {
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

    Ok(admission)
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

    // Pin the PTY peer once — holding the backing keeps the peer's slot
    // from being freed or reused for the whole write, so the cached slot
    // indices below stay valid. A failed upgrade means the peer is gone:
    // the write has nowhere to go (the master strongly holds its slave,
    // so only the slave→master direction can observe this).
    let (_peer_pin, peer_slave_slot, peer_master_slot): (
        Option<slopos_ostd::KArc<super::backing::TtyBacking>>,
        Option<usize>,
        Option<usize>,
    ) = {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(tty) => match &tty.driver {
                TtyDriverKind::PtyMaster { peer } => match peer.upgrade() {
                    Some(pin) => {
                        let s = pin.index().0 as usize;
                        (Some(pin), Some(s), None)
                    }
                    None => return Err(TtyError::HungUp),
                },
                TtyDriverKind::PtySlave { peer } => match peer.upgrade() {
                    Some(pin) => {
                        let m = pin.index().0 as usize;
                        (Some(pin), None, Some(m))
                    }
                    None => return Err(TtyError::HungUp),
                },
                _ => (None, None, None),
            },
            None => (None, None, None),
        }
    };
    let mut pos = 0;
    while pos < data.len() {
        let next_priority_byte = peer_slave_slot.map(|_| data[pos]);
        let admission = match wait_for_write_ready(
            slot,
            peer_slave_slot,
            peer_master_slot,
            nonblock,
            next_priority_byte,
        ) {
            Ok(admission) => admission,
            Err(err) => return if pos > 0 { Ok(pos) } else { Err(err) },
        };
        let input_limit = if admission == WriteAdmission::PriorityControlOnly {
            pos + 1
        } else {
            data.len()
        };

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

            while pos < input_limit {
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
            let _inflight = InflightGuard::new(slot, out_len);
            let written = write_driver_unlocked(driver_id, &out_buf[..out_len]);
            written
        };
        if driver_written < out_len {
            break;
        }
        BUS.publish(tty_output_event(slot));
        if admission == WriteAdmission::PriorityControlOnly {
            return Ok(pos);
        }
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
    // SysRq (Ctrl+T) marked on an input path: fire the task dump here — the
    // idle-loop callback runs in task context with no TTY locks held, the
    // one place every input path (ISR, serial drain, PTY) can safely defer
    // the pool walk to.
    if super::SYSRQ_PENDING.swap(false, core::sync::atomic::Ordering::AcqRel) {
        slopos_kernel_services::driver_runtime::debug_dump_tasks();
    }
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
