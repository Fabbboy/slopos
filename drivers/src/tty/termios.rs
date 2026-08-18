//! TTY termios configuration — get/set termios, window size, line discipline,
//! output drain, and control ioctls (TCFLSH, TCSBRK, TCXONC).

use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIGTTOU, SIGWINCH};
use slopos_abi::syscall::{B0, CBAUD, CcIndex, InputFlags, UserTermios, UserWinsize};

use slopos_kernel_services::driver_runtime::{
    current_task_id, current_task_pgid, current_task_sid, is_current_signal_blocked_or_ignored,
    is_pgrp_orphaned, scheduler_is_enabled, signal_process_group,
};

use super::driver::TtyDriverKind;
use super::ldisc::LdiscKind;
use super::lifecycle::hangup;
use super::output::{self, WriteNesting};
use super::pty;
use super::session::ForegroundCheck;
use super::table::{TTY_OUTPUT_INFLIGHT, TTY_SLOTS, tty_output_event};
use super::{MAX_TTYS, PostLockWork, TtyError, TtyFlags, TtyIndex};
use slopos_ostd::sync::{BUS, WaitAbort};

#[derive(Clone, Copy)]
pub(super) enum TermiosSetMode {
    Now,
    Drain,
    DrainAndFlushInput,
}

fn cflag_to_speed(cflag: u32) -> u32 {
    use slopos_abi::syscall::*;
    match cflag & CBAUD {
        B0 => 0,
        B50 => 50,
        B75 => 75,
        B110 => 110,
        B134 => 134,
        B150 => 150,
        B200 => 200,
        B300 => 300,
        B600 => 600,
        B1200 => 1200,
        B1800 => 1800,
        B2400 => 2400,
        B4800 => 4800,
        B9600 => 9600,
        B19200 => 19200,
        B38400 => 38400,
        B57600 => 57600,
        B115200 => 115200,
        B230400 => 230400,
        B460800 => 460800,
        B500000 => 500000,
        B576000 => 576000,
        B921600 => 921600,
        B1000000 => 1000000,
        B1152000 => 1152000,
        B1500000 => 1500000,
        B2000000 => 2000000,
        B2500000 => 2500000,
        B3000000 => 3000000,
        B3500000 => 3500000,
        B4000000 => 4000000,
        _ => 0,
    }
}

/// Populates `c_ispeed`/`c_ospeed` from the baud rate encoded in `c_cflag`
/// before returning to userland.
#[must_use]
pub fn get_termios(idx: TtyIndex) -> Result<UserTermios, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => {
            let mut t = *tty.ldisc.termios();
            let speed = cflag_to_speed(t.c_cflag.bits());
            t.c_ispeed = speed;
            t.c_ospeed = speed;
            Ok(t)
        }
        None => Err(TtyError::NotAllocated),
    }
}

/// The single authoritative drain path: `tcsbrk(arg > 0)` and
/// `set_termios_mode(Drain | DrainAndFlushInput)` both delegate here, and no
/// other path may implement drain logic of its own.
///
/// Staged echo has no drainer of its own — whichever CPU stages it flushes it —
/// so this drives the flush rather than waiting on work nobody is committed to
/// doing.
fn wait_output_idle(idx: TtyIndex) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(tty) if tty.flags.contains(TtyFlags::HUNG_UP) => return Ok(()),
            Some(_) => {}
            None => return Err(TtyError::NotAllocated),
        }
    }

    if scheduler_is_enabled() != 0 {
        loop {
            // Subscribed before the flush, so the event the flush publishes
            // cannot land in the gap between flushing and parking.
            let waiter = BUS.subscribe(tty_output_event(slot));
            output::flush_echo(slot, WriteNesting::Toplevel);
            match waiter.wait_event_interruptible(|| output_settled(slot)) {
                Ok(()) | Err(WaitAbort::Timeout) => {}
                // No blocking surface: nothing can make progress on this
                // task's behalf.
                Err(WaitAbort::NoRuntime) => return Ok(()),
                Err(WaitAbort::Interrupted) => return Err(TtyError::Restart),
                // Never Restart for a dying task: the killed bit is not
                // deliverable, so the syscall would restart forever.
                Err(WaitAbort::Killed) => return Err(TtyError::SignalInterrupt),
            }
            if output_settled(slot) {
                return Ok(());
            }
        }
    }

    loop {
        output::flush_echo(slot, WriteNesting::Toplevel);
        if output_settled(slot) {
            return Ok(());
        }
        core::hint::spin_loop();
    }
}

/// A vanished or hung-up slot settles vacuously.
///
/// The staged count and the in-flight count are read in one critical section
/// because a byte moves between them under this very lock; sampling either one
/// outside it would let a mid-hand-off flush show both as empty.
fn output_settled(slot: usize) -> bool {
    let guard = TTY_SLOTS[slot].lock();
    let drained_locally = match guard.as_ref() {
        Some(tty) if tty.flags.contains(TtyFlags::HUNG_UP) => return true,
        Some(tty) => tty.ldisc.echo_staged() == 0 && !tty.driver.output_pending(),
        None => return true,
    };
    drained_locally && TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire) == 0
}

/// Per POSIX a background process calling `tcsetattr` receives `SIGTTOU` unless
/// the signal is blocked or ignored; an orphaned background group gets `EIO`
/// instead, since no parent could continue it after a stop.
pub(super) fn set_termios_mode(
    idx: TtyIndex,
    t: &UserTermios,
    mode: TermiosSetMode,
) -> Result<(), TtyError> {
    // `c_cflag`'s CBAUD bits are authoritative: `c_ispeed`/`c_ospeed` are
    // informational output of `get_termios` and never override them here.
    let merged = *t;

    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // POSIX: a state-changing ioctl on a hung-up TTY returns EIO.
    {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.flags.contains(TtyFlags::HUNG_UP) {
                return Err(TtyError::HungUp);
            }
        }
    }

    // Task id 0 is an early-boot write with no process group to check.
    let task_id = current_task_id();
    if task_id != 0 {
        let caller_pgid = current_task_pgid();
        let caller_sid = current_task_sid();

        let check_result = {
            let guard = TTY_SLOTS[slot].lock();
            match guard.as_ref() {
                Some(tty) => {
                    // POSIX §11.1.4: tcsetattr always enforces the foreground
                    // check, unlike write(), which only checks under TOSTOP.
                    tty.session.check_write(caller_pgid, caller_sid, true)
                }
                None => return Err(TtyError::NotAllocated),
            }
        };

        match check_result {
            ForegroundCheck::BackgroundWrite => {
                if !is_current_signal_blocked_or_ignored(SIGTTOU) {
                    if is_pgrp_orphaned(caller_pgid, caller_sid) {
                        return Err(TtyError::OrphanedProcessGroup);
                    }
                    if caller_pgid != 0 {
                        let _ = signal_process_group(caller_pgid, SIGTTOU);
                    }
                    return Err(TtyError::SignalInterrupt);
                }
            }
            ForegroundCheck::DeniedCrossSession => {
                return Err(TtyError::CrossSessionDenied);
            }
            _ => {}
        }
    }

    if matches!(
        mode,
        TermiosSetMode::Drain | TermiosSetMode::DrainAndFlushInput
    ) {
        wait_output_idle(idx)?;
    }

    let mut deferred = PostLockWork::new();
    let mut defer_hangup = false;
    let result = {
        let mut guard = TTY_SLOTS[slot].lock();
        match guard.as_mut() {
            Some(tty) => {
                let old_ixon = tty.ldisc.termios().c_iflag.contains(InputFlags::IXON);
                let new_ixon = merged.c_iflag.contains(InputFlags::IXON);

                if matches!(mode, TermiosSetMode::DrainAndFlushInput) {
                    tty.ldisc.flush_input();
                    deferred.add_packet_event(idx, slopos_abi::syscall::TIOCPKT_FLUSHREAD);
                }
                tty.ldisc.set_termios(&merged);
                tty.driver.set_termios(&merged);
                defer_hangup = (merged.c_cflag.bits() & CBAUD) == B0;

                if !old_ixon && new_ixon {
                    deferred.add_packet_event(idx, slopos_abi::syscall::TIOCPKT_DOSTOP);
                } else if old_ixon && !new_ixon {
                    deferred.add_packet_event(idx, slopos_abi::syscall::TIOCPKT_NOSTOP);
                }

                Ok(())
            }
            None => Err(TtyError::NotAllocated),
        }
    };

    deferred.execute();

    if defer_hangup {
        hangup(idx);
    }

    result
}

#[must_use]
pub fn set_termios(idx: TtyIndex, t: &UserTermios) -> Result<(), TtyError> {
    set_termios_mode(idx, t, TermiosSetMode::Now)
}

#[must_use]
pub fn set_termios_wait(idx: TtyIndex, t: &UserTermios) -> Result<(), TtyError> {
    set_termios_mode(idx, t, TermiosSetMode::Drain)
}

#[must_use]
pub fn set_termios_flush(idx: TtyIndex, t: &UserTermios) -> Result<(), TtyError> {
    set_termios_mode(idx, t, TermiosSetMode::DrainAndFlushInput)
}

/// Non-blocking drain query, for test observability and `TIOCOUTQ`; callers
/// that must block until drain completes use `wait_output_idle`.
#[must_use]
pub fn is_output_idle(idx: TtyIndex) -> Result<bool, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    {
        let guard = TTY_SLOTS[slot].lock();
        if guard.as_ref().is_none() {
            return Err(TtyError::NotAllocated);
        }
    }
    Ok(output_settled(slot))
}

#[must_use]
pub fn get_ldisc(idx: TtyIndex) -> Result<u32, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => Ok(tty.ldisc.id()),
        None => Err(TtyError::NotAllocated),
    }
}

/// An unsupported `ldisc_id` leaves the current discipline untouched — no
/// flush, no state change — because `LdiscKind::from_id` constructs the
/// replacement before anything is mutated.
#[must_use]
pub fn set_ldisc(idx: TtyIndex, ldisc_id: u32) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let did_flush;
    let result = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(tty) => tty,
            None => return Err(TtyError::NotAllocated),
        };

        if tty.flags.contains(TtyFlags::HUNG_UP) {
            return Err(TtyError::HungUp);
        }

        if tty.ldisc.id() == ldisc_id {
            let mut termios = *tty.ldisc.termios();
            termios.c_line = ldisc_id as u8;
            tty.ldisc.set_termios(&termios);
            tty.driver.set_termios(tty.ldisc.termios());
            return Ok(());
        }

        let mut termios = *tty.ldisc.termios();
        termios.c_line = ldisc_id as u8;
        let new_ldisc = match LdiscKind::from_id(ldisc_id, termios) {
            Ok(Some(ld)) => ld,
            Ok(None) => return Err(TtyError::UnsupportedLineDiscipline),
            Err(_) => return Err(TtyError::OutOfMemory),
        };

        tty.ldisc.flush_input();
        did_flush = true;
        tty.ldisc = new_ldisc;
        tty.driver.set_termios(tty.ldisc.termios());
        Ok(())
    };

    if did_flush {
        pty::queue_packet_event(idx, slopos_abi::syscall::TIOCPKT_FLUSHREAD);
    }

    result
}

#[must_use]
pub fn get_winsize(idx: TtyIndex) -> Result<UserWinsize, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => Ok(tty.winsize),
        None => Err(TtyError::NotAllocated),
    }
}

/// A changed size sends SIGWINCH to the foreground process group so
/// applications re-query their dimensions.
#[must_use]
pub fn set_winsize(idx: TtyIndex, ws: &UserWinsize) -> Result<(), TtyError> {
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

    // Window size is a property of the terminal pair: setting it on either PTY
    // end updates both views, and SIGWINCH targets the slave side's foreground
    // job, because the session lives on the slave where the shell did TIOCSCTTY.
    let mut deferred = PostLockWork::new();
    // Pins the peer slot alive across the second lock below; the bool records
    // whether the ioctl targeted the master.
    let peer_pin;
    let mut changed;
    {
        let mut guard = TTY_SLOTS[slot].lock();
        match guard.as_mut() {
            Some(tty) => {
                let old = tty.winsize;
                tty.winsize = *ws;
                changed = old.ws_row != ws.ws_row || old.ws_col != ws.ws_col;
                match &tty.driver {
                    TtyDriverKind::PtyMaster { peer } => {
                        peer_pin = peer.upgrade().map(|pin| (pin, true));
                    }
                    TtyDriverKind::PtySlave { peer } => {
                        peer_pin = peer.upgrade().map(|pin| (pin, false));
                        if changed {
                            if let Some(pg) = tty.session.fg_pgrp_handle() {
                                deferred.add_signal(pg, SIGWINCH);
                            }
                        }
                    }
                    _ => {
                        peer_pin = None;
                        if changed {
                            if let Some(pg) = tty.session.fg_pgrp_handle() {
                                deferred.add_signal(pg, SIGWINCH);
                            }
                        }
                    }
                }
            }
            None => return Err(TtyError::NotAllocated),
        }
    }

    if let Some((peer, target_is_master)) = peer_pin {
        let peer_slot = peer.index().0 as usize;
        let mut guard = TTY_SLOTS[peer_slot].lock();
        if let Some(peer_tty) = guard.as_mut() {
            let old = peer_tty.winsize;
            peer_tty.winsize = *ws;
            changed = changed || old.ws_row != ws.ws_row || old.ws_col != ws.ws_col;
            if target_is_master && changed {
                // The peer is the slave: signal ITS foreground job.
                if let Some(pg) = peer_tty.session.fg_pgrp_handle() {
                    deferred.add_signal(pg, SIGWINCH);
                }
            }
        }
    }

    deferred.execute();
    Ok(())
}

/// Nothing here reaches a driver-level hardware TX FIFO.
///
/// The IXOFF stop is re-armed rather than dropped with the rest: the stop
/// latches when generated, so discarding it would leave the peer never told to
/// stop and — the latch still set — never told to resume either.
///
/// Zeroing the in-flight count also discards bytes a concurrent emission still
/// owns, so a drain racing this flush under-reports the slot.
fn discard_pending_output(slot: usize) {
    {
        let mut guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_mut() {
            tty.ldisc.echo_discard();
            tty.ldisc.ixoff_rearm();
        }
    }
    TTY_OUTPUT_INFLIGHT[slot].store(0, Ordering::Release);
}

pub fn tcflush(idx: TtyIndex, queue: i32) -> Result<(), TtyError> {
    use slopos_abi::syscall::{TCIFLUSH, TCIOFLUSH, TCOFLUSH};
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Pinned inside the lock; the wake is delivered outside it, for lock order.
    let mut unthrottled_peer = None;

    match queue {
        TCIFLUSH | TCIOFLUSH => {
            {
                let mut guard = TTY_SLOTS[slot].lock();
                if let Some(tty) = guard.as_mut() {
                    tty.ldisc.flush_input();

                    // The buffer is empty after the flush, so the throttle
                    // condition no longer holds.
                    if tty.flags.contains(TtyFlags::THROTTLED) {
                        tty.flags.remove(TtyFlags::THROTTLED);
                        if let TtyDriverKind::PtySlave { ref peer } = tty.driver {
                            unthrottled_peer = peer.upgrade();
                        }
                    }
                } else {
                    return Err(TtyError::NotAllocated);
                }
            }
            if queue == TCIOFLUSH {
                discard_pending_output(slot);
            }
        }
        TCOFLUSH => {
            discard_pending_output(slot);
        }
        _ => return Err(TtyError::InvalidArg),
    }

    if let Some(master) = unthrottled_peer {
        // Poll waiters park on the output queue too, so one publish covers both.
        BUS.publish(tty_output_event(master.index().0 as usize));
    }

    Ok(())
}

/// `tcsendbreak()` / `TCSBRK`: `arg == 0` sends a ~0.25 s break (a no-op on
/// PTYs and QEMU serial); `arg > 0` is `tcdrain()`, delegating to
/// [`wait_output_idle`].
pub fn tcsbrk(idx: TtyIndex, arg: i32) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(tty) if tty.flags.contains(TtyFlags::HUNG_UP) => return Err(TtyError::HungUp),
            Some(_) => {}
            None => return Err(TtyError::NotAllocated),
        }
    }

    if arg > 0 {
        wait_output_idle(idx)?;
    }
    Ok(())
}

/// `tcflow()` / `TCXONC`.
///
/// - `TCOOFF` / `TCOON`: suspend or resume output, so the write path blocks
///   (or returns EAGAIN on a non-blocking FD).
/// - `TCIOFF` / `TCION`: transmit the `VSTOP` / `VSTART` character.
pub fn tcxonc(idx: TtyIndex, action: i32) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    use slopos_abi::syscall::{TCIOFF, TCION, TCOOFF, TCOON};

    match action {
        TCOOFF => {
            let was_stopped = {
                let mut guard = TTY_SLOTS[slot].lock();
                let tty = guard.as_mut().ok_or(TtyError::NotAllocated)?;
                let prev = tty.flags.contains(TtyFlags::OUTPUT_STOPPED);
                tty.flags.insert(TtyFlags::OUTPUT_STOPPED);
                prev
            };
            if !was_stopped {
                pty::queue_packet_event(idx, slopos_abi::syscall::TIOCPKT_STOP);
            }
            Ok(())
        }
        TCOON => {
            let was_stopped = {
                let mut guard = TTY_SLOTS[slot].lock();
                let tty = guard.as_mut().ok_or(TtyError::NotAllocated)?;
                let prev = tty.flags.contains(TtyFlags::OUTPUT_STOPPED);
                tty.flags.remove(TtyFlags::OUTPUT_STOPPED);
                prev
            };
            BUS.publish(tty_output_event(slot));
            if was_stopped {
                pty::queue_packet_event(idx, slopos_abi::syscall::TIOCPKT_START);
            }
            Ok(())
        }
        TCIOFF => {
            let (driver_id, stop_byte) = {
                let guard = TTY_SLOTS[slot].lock();
                let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
                let stop = tty.ldisc.termios().c_cc[CcIndex::Vstop.as_usize()];
                (tty.driver.id(), stop)
            };
            output::write_processed(slot, driver_id, &[stop_byte], WriteNesting::Toplevel);
            Ok(())
        }
        TCION => {
            let (driver_id, start_byte) = {
                let guard = TTY_SLOTS[slot].lock();
                let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
                let start = tty.ldisc.termios().c_cc[CcIndex::Vstart.as_usize()];
                (tty.driver.id(), start)
            };
            output::write_processed(slot, driver_id, &[start_byte], WriteNesting::Toplevel);
            Ok(())
        }
        _ => Err(TtyError::InvalidArg),
    }
}
