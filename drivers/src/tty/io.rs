//! TTY I/O paths — read, write, push_input, hardware drain, data queries,
//! and the idle-loop input callback.
//!
//! Echo is staged under the slot lock and drained by `super::output` under
//! `TTY_WRITE_LOCKS[slot]` after the guard drops.  That staging is what keeps
//! the write lock outside every slot lock, and it serialises echo against user
//! output at the byte level (POSIX §11.1.9).

use core::ffi::c_int;
use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIGTTIN, SIGTTOU};
use slopos_abi::syscall::LocalFlags;

use super::driver::{InputEvent, TtyDriverKind};
use super::ldisc::{self, BatchResult, OutputAction};
use super::output::{self, WriteNesting};
use super::session::ForegroundCheck;
use super::table::{TTY_OUTPUT_INFLIGHT, TTY_SLOTS, tty_input_event, tty_output_event};
use super::{MAX_TTYS, PacketEvents, PostLockWork, Tty, TtyError, TtyFlags, TtyIndex};
use slopos_kernel_services::driver_runtime::{
    current_task_id, current_task_pgid, current_task_sid, is_current_signal_blocked_or_ignored,
    is_pgrp_orphaned, register_idle_wakeup_callback, scheduler_is_enabled, signal_process_group,
};
use slopos_ostd::sync::{BUS, WaitAbort};

impl Tty {
    /// Drain pending hardware input into the line discipline.
    ///
    /// Caller holds the per-TTY lock.  Echo, any IXOFF byte and any generated
    /// signal are registered with `deferred` for emission once the caller drops
    /// the slot guard.
    pub(crate) fn drain_hw_input_locked(&mut self, deferred: &mut PostLockWork) {
        let mut scratch = [0u8; 64];
        let count = self.driver.drain_input(&mut scratch);
        let mut events = [InputEvent::normal(0); 64];
        // Raw bytes go straight to the ldisc: pre-mapping CR/NL/DEL here would
        // bypass c_iflag semantics (IGNCR could not discard an already-mapped CR).
        for i in 0..count {
            events[i] = InputEvent::normal(scratch[i]);
        }

        let batch = self.ldisc.receive_buf(&events[..count]);
        if let Some(xoff) = self.ldisc.ixoff_check_xoff() {
            self.ldisc.echo_stage(&[xoff]);
        }
        self.queue_echo_flush(deferred, WriteNesting::Toplevel);

        if let Some((sig, _)) = batch.signal {
            if let Some(pg) = self.session.fg_pgrp_handle() {
                deferred.add_signal(pg, sig);
            }
        }
    }

    /// Register any staged echo for emission after the slot guard drops.
    #[inline]
    pub(crate) fn queue_echo_flush(&self, deferred: &mut PostLockWork, nesting: WriteNesting) {
        if !self.ldisc.echo_is_empty() {
            deferred.request_echo_flush(self.index.0 as usize, nesting);
        }
    }
}

pub use super::pty::{
    get_packet_mode, get_pty_lock, get_pty_number, is_pty_slave, is_slave_locked, pty_alloc,
    queue_packet_event, set_packet_mode, set_pty_lock,
};

pub fn push_input<E: Into<InputEvent>>(idx: TtyIndex, event: E) {
    let event = event.into();
    push_input_batch(idx, core::slice::from_ref(&event));
}

/// Feed input to a TTY with no TTY write lock held — the keyboard ISR, a test.
pub fn push_input_batch(idx: TtyIndex, events: &[InputEvent]) {
    push_input_batch_nested(idx, events, WriteNesting::Toplevel);
}

/// [`push_input_batch`] for a PTY master write, which reaches the slave with the
/// master's own write lock still held — the acquisition
/// [`WriteNesting::PeerNested`] names.
pub(crate) fn push_input_batch_nested(idx: TtyIndex, events: &[InputEvent], nesting: WriteNesting) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS || events.is_empty() {
        return;
    }

    let mut deferred = PostLockWork::new();
    // ISIG output flush (NOFLSH clear): the slave's undelivered output lives in
    // the peer master's read buffer, discarded after the slave lock drops (peer
    // lock ordering) and before the caret echo, so `^C` lands in an empty
    // buffer. The held backing pins the master's slot until the flush lands.
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
            tty.ldisc.echo_stage(&[xoff]);
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

        tty.queue_echo_flush(&mut deferred, nesting);

        if let Some((sig, flush)) = batch.signal {
            if let Some(pg) = tty.session.fg_pgrp_handle() {
                deferred.add_signal(pg, sig);
            }
            // Unless NOFLSH, a signal char discards the foreground job's pending
            // I/O. The signal is posted ahead of the output-event wakes in
            // `deferred.execute()`, so a writer blocked on the flushed queue
            // observes it the moment its wait predicate re-runs.
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
            // TCOFLUSH: drop the slave's inflight accounting with the buffer.
            // PTY-only — synchronous backends (vconsole, serial) complete output
            // inline, so a reset there would only corrupt the counter.
            TTY_OUTPUT_INFLIGHT[slot].store(0, Ordering::Release);
            deferred.wake_output_and_poll(master_slot);
        }
    }

    if wake {
        notify_input_ready(idx);
    }

    deferred.execute();
}

fn notify_input_ready(idx: TtyIndex) {
    if scheduler_is_enabled() == 0 {
        return;
    }
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
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
/// Not-yet-foreground is transient, so a non-blocking probe parks as
/// `WouldBlock` and self-heals — surfacing `BackgroundRead` (-EIO) would poison
/// an armed slop-ring `OP_READ` permanently and re-raise SIGTTIN on every
/// re-probe.  A blocking read keeps the POSIX `BackgroundRead` surface.
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
/// the read.  Nothing else wakes a slave writer parked in
/// `wait_for_write_ready`'s `peer_master` arm — the master never enters the
/// `THROTTLED`/`no_room` states the arms below key on — and a slave `tcdrain`
/// parks on the same edge, so the full→not-full edge wakes both slots.
fn drain_and_recover(
    tty: &mut Tty,
    slot: usize,
    master_was_full: bool,
    deferred: &mut PostLockWork,
) -> bool {
    let mut woke_peers = false;

    if master_was_full {
        if let TtyDriverKind::PtyMaster { peer } = &tty.driver {
            deferred.wake_output_and_poll(slot);
            if let Some(slave) = peer.upgrade() {
                deferred.wake_output_and_poll(slave.index().0 as usize);
            }
            woke_peers = true;
        }
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
        tty.ldisc.echo_stage(&[xon]);
        tty.queue_echo_flush(deferred, WriteNesting::Toplevel);
    }
    let _ = drain_and_recover(tty, slot, was_full, deferred);
    Some(Ok(1 + got))
}

/// Read cooked data from a specific TTY.
///
/// `TtySession::check_read()` is the sole read-side gate; a background process
/// receives `SIGTTIN` instead of silently blocking.
#[must_use]
pub fn read(idx: TtyIndex, buf: &mut [u8], nonblock: bool) -> Result<usize, TtyError> {
    read_with_attach(idx, buf, nonblock, true)
}

/// `_auto_attach` is dead: a read never claims a controlling terminal.  The
/// parameter stays for ABI compatibility with the kernel services trait
/// (`read_cooked_with_attach`).

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

            tty.drain_hw_input_locked(&mut deferred);

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
                        tty.ldisc.echo_stage(&[xon]);
                        tty.queue_echo_flush(&mut deferred, WriteNesting::Toplevel);
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

            // Peer close and hangup end the read without discarding what this
            // iteration collected or the work it staged.
            if (tty.flags.contains(TtyFlags::PEER_CLOSED) || tty.flags.contains(TtyFlags::HUNG_UP))
                && !tty.ldisc.has_data()
            {
                drop(guard);
                deferred.execute();
                return Ok(total);
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

        // `wait_core` releases the queue's own lock before calling this, so the
        // predicate may take the slot lock and drain hardware.
        let wait_condition = || {
            let mut wd = PostLockWork::new();
            let result = {
                let mut guard = TTY_SLOTS[slot].lock();
                match guard.as_mut() {
                    Some(tty) => {
                        let denied = enforce_access
                            && matches!(
                                tty.session.check_read(caller_pgid, caller_sid),
                                ForegroundCheck::BackgroundRead
                                    | ForegroundCheck::DeniedCrossSession
                            );
                        if denied {
                            false
                        } else {
                            tty.drain_hw_input_locked(&mut wd);
                            tty.flags.contains(TtyFlags::HUNG_UP)
                                || tty.flags.contains(TtyFlags::PEER_CLOSED)
                                || tty.ldisc.has_data()
                        }
                    }
                    None => true,
                }
            };
            wd.execute();
            result
        };

        let waited = match wait_timeout_ms {
            Some(timeout_ms) => BUS
                .subscribe(tty_input_event(slot))
                .wait_event_interruptible_timeout(wait_condition, timeout_ms),
            None => BUS
                .subscribe(tty_input_event(slot))
                .wait_event_interruptible(wait_condition),
        };
        match waited {
            Ok(()) => {}
            Err(WaitAbort::Timeout | WaitAbort::NoRuntime) => {
                return if total > 0 { Ok(total) } else { Ok(0) };
            }
            Err(WaitAbort::Interrupted) => {
                return if total > 0 {
                    Ok(total)
                } else {
                    Err(TtyError::Restart)
                };
            }
            // Never Restart for a dying task: the killed bit is not deliverable,
            // so the restart would loop forever.
            Err(WaitAbort::Killed) => {
                return if total > 0 {
                    Ok(total)
                } else {
                    Err(TtyError::SignalInterrupt)
                };
            }
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
            match BUS
                .subscribe(tty_output_event(peer_slot))
                .wait_event_interruptible(|| {
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
                }) {
                Ok(()) | Err(WaitAbort::NoRuntime) => {}
                Err(WaitAbort::Interrupted) => return Err(TtyError::Restart),
                Err(WaitAbort::Killed) => return Err(TtyError::SignalInterrupt),
                Err(WaitAbort::Timeout) => {}
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
            // Interruptible: this predicate can stay false forever if the master
            // stops draining, and delivery only happens at syscall exit.
            match BUS
                .subscribe(tty_output_event(master_slot))
                .wait_event_interruptible(|| {
                    let guard = TTY_SLOTS[master_slot].lock();
                    match guard.as_ref() {
                        Some(tty) => {
                            !tty.ldisc.input_full()
                                || tty.flags.contains(TtyFlags::HUNG_UP)
                                || tty.flags.contains(TtyFlags::PEER_CLOSED)
                        }
                        None => true,
                    }
                }) {
                Ok(()) | Err(WaitAbort::NoRuntime) => {}
                Err(WaitAbort::Interrupted) => return Err(TtyError::Restart),
                Err(WaitAbort::Killed) => return Err(TtyError::SignalInterrupt),
                Err(WaitAbort::Timeout) => {}
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
        match BUS
            .subscribe(tty_output_event(slot))
            .wait_event_interruptible(|| {
                let guard = TTY_SLOTS[slot].lock();
                match guard.as_ref() {
                    Some(tty) => {
                        !tty.ldisc.is_stopped() && !tty.flags.contains(TtyFlags::OUTPUT_STOPPED)
                    }
                    None => true,
                }
            }) {
            Ok(()) | Err(WaitAbort::NoRuntime) => {}
            Err(WaitAbort::Interrupted) => return Err(TtyError::Restart),
            Err(WaitAbort::Killed) => return Err(TtyError::SignalInterrupt),
            Err(WaitAbort::Timeout) => {}
        }
    }

    Ok(admission)
}

/// Write bytes to a specific TTY.
///
/// Output processing (`c_oflag`, e.g. OPOST + ONLCR) runs under the per-TTY
/// lock into a stack buffer; the lock is dropped before the hardware write, so
/// slow serial I/O cannot block other TTYs.  With `TOSTOP` set, a background
/// process receives `SIGTTOU` rather than writing.
#[must_use]
pub fn write(idx: TtyIndex, data: &[u8], nonblock: bool) -> Result<usize, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.flags.contains(TtyFlags::HUNG_UP) {
                return Err(TtyError::HungUp);
            }
        }
    }

    check_write_foreground(slot)?;

    // Each input byte can expand to two output bytes (NL → CR+NL under ONLCR).
    const OUT_BUF_CAP: usize = 256;

    // Pinning the peer's backing keeps its slot from being freed or reused, so
    // the cached slot indices below stay valid for the whole write.
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
                if out_len >= OUT_BUF_CAP - 8 {
                    break;
                }
            }
        }
        let driver_written =
            output::write_processed(slot, driver_id, &out_buf[..out_len], WriteNesting::Toplevel);
        if driver_written < out_len {
            break;
        }
        if admission == WriteAdmission::PriorityControlOnly {
            return Ok(pos);
        }
    }

    Ok(pos)
}

/// Check whether a TTY has cooked data available for reading.
pub fn has_data(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    let mut deferred = PostLockWork::new();
    let result = {
        let mut guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_mut() {
            tty.drain_hw_input_locked(&mut deferred);
            tty.ldisc.has_data()
        } else {
            return false;
        }
    };
    deferred.execute();
    result
}

/// Bytes available for reading (FIONREAD / TIOCINQ); drains hardware input first.
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
            tty.drain_hw_input_locked(&mut deferred);
            tty.ldisc.bytes_available()
        } else {
            return Err(TtyError::NotAllocated);
        }
    };
    deferred.execute();
    Ok(count)
}

/// Bytes queued for output (`TIOCOUTQ`): ldisc-staged echo, the per-TTY
/// inflight counter, and driver-level pending output.
#[must_use]
pub fn output_queued_bytes(idx: TtyIndex) -> Result<usize, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    // One critical section: a byte moves from staged to inflight under this
    // lock, so sampling them apart could miss it in both counts.
    let (staged, inflight, driver_pending) = {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            (
                tty.ldisc.echo_staged(),
                TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire) as usize,
                tty.driver.output_pending_bytes(),
            )
        } else {
            return Err(TtyError::NotAllocated);
        }
    };
    Ok(staged + inflight + driver_pending)
}

/// Idle-loop callback: drain hardware input and wake blocked readers.
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
                tty.drain_hw_input_locked(&mut deferred);
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
