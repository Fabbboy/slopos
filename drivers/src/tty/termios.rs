//! TTY termios configuration — get/set termios, window size, line discipline,
//! output drain, and control ioctls (TCFLSH, TCSBRK, TCXONC).
//!
//! decomposition: extracted from `mod.rs` to group all terminal
//! attribute management into a single focused module.

use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIGTTOU, SIGWINCH};
use slopos_abi::syscall::{B0, CBAUD, CcIndex, InputFlags, UserTermios, UserWinsize};

use slopos_kernel_services::driver_runtime::{
    current_task_id, current_task_pgid, current_task_sid, has_pending_signal,
    is_current_signal_blocked_or_ignored, is_pgrp_orphaned, scheduler_is_enabled,
    signal_process_group,
};

use super::driver::{TtyDriverKind, write_driver_unlocked};
use super::ldisc::LdiscKind;
use super::lifecycle::hangup;
use super::pty;
use super::session::ForegroundCheck;
use super::table::{TTY_OUTPUT_INFLIGHT, TTY_SLOTS, TTY_WRITE_LOCKS, tty_output_event};
use super::{MAX_TTYS, PostLockWork, TtyError, TtyFlags, TtyIndex};
use slopos_ostd::sync::BUS;

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(super) enum TermiosSetMode {
    Now,
    Drain,
    DrainAndFlushInput,
}

// ---------------------------------------------------------------------------
// Baud rate mapping
// ---------------------------------------------------------------------------

/// Map baud rate bits from `c_cflag & CBAUD` to numeric speed.
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

// ---------------------------------------------------------------------------
// Termios get / set
// ---------------------------------------------------------------------------

/// Get termios for a specific TTY.
///
/// Populates `c_ispeed`/`c_ospeed` from the baud rate
/// encoded in `c_cflag` before returning to userland.
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

/// Wait until all in-flight output has been transmitted to the hardware.
///
/// # Drain Contract
///
/// This is the **single authoritative drain path** for the TTY subsystem.
/// Both `tcsbrk(arg > 0)` and `set_termios_mode(Drain | DrainAndFlushInput)`
/// delegate to this function.  No other code path may independently
/// implement drain logic — all drain consumers MUST go through here.
///
/// ## Definition of "idle"
///
/// Output is considered **idle** when ANY of the following are true:
///
///   - The TTY is hung up (`tty.flags.contains(TtyFlags::HUNG_UP) == true`).  Hangup discards all
///     pending output, so the drain is vacuously complete.
///   - The slot has been deallocated (`None`).  Same reasoning.
///   - BOTH of these hold simultaneously:
///     1. `TTY_OUTPUT_INFLIGHT[slot] == 0` — no `write()` is between
///        ldisc processing and driver transmission.
///     2. `!tty.driver.output_pending()` — the driver backend has no
///        un-transmitted bytes in its hardware FIFO.
///
/// ## Edge-case behavior
///
///   - **Invalid index** (`>= MAX_TTYS`): returns `Err(InvalidIndex)`.
///   - **Unallocated slot**: returns `Err(NotAllocated)`.
///   - **Signal interruption**: the current implementation does NOT check
///     for pending signals; drain is NOT interruptible.  Callers that
///     need interruptibility should add `wait_event_interruptible`.
///
/// ## Synchronous vs asynchronous drivers
///
/// For synchronous backends (serial, vconsole) both conditions are
/// trivially satisfied because the driver blocks until each byte is on
/// the wire.  For future async/interrupt-driven drivers, callers will
/// genuinely sleep on the TTY output event until the TX FIFO empties.
///
/// ## Scheduler awareness
///
/// The function sleeps on `KernelEvent::TtyOutput` if the scheduler is
/// available; otherwise it busy-polls (pre-scheduler boot path only).
fn wait_output_idle(idx: TtyIndex) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Quick validation: ensure the slot is allocated (and not hung up).
    {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(tty) if tty.flags.contains(TtyFlags::HUNG_UP) => return Ok(()), // hangup discards output
            Some(_) => {} // slot alive, proceed
            None => return Err(TtyError::NotAllocated),
        }
    }

    // Fast path: if nothing is in-flight and driver has no pending output,
    // return immediately without touching the wait queue.
    if TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire) == 0 {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.flags.contains(TtyFlags::HUNG_UP) || !tty.driver.output_pending() {
                return Ok(());
            }
        } else {
            return Ok(()); // slot gone — drain vacuously satisfied
        }
    }

    // Slow path: wait until drain completes.
    if scheduler_is_enabled() != 0 {
        BUS.subscribe(tty_output_event(slot)).wait_event(|| {
            if has_pending_signal() {
                return true;
            }
            if TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire) != 0 {
                return false;
            }
            let guard = TTY_SLOTS[slot].lock();
            match guard.as_ref() {
                Some(tty) => tty.flags.contains(TtyFlags::HUNG_UP) || !tty.driver.output_pending(),
                None => true, // slot gone — drain vacuously satisfied
            }
        });
        if has_pending_signal() {
            return Err(TtyError::Restart);
        }
    } else {
        // Pre-scheduler fallback: busy-poll (very early boot only).
        loop {
            if TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire) == 0 {
                let guard = TTY_SLOTS[slot].lock();
                match guard.as_ref() {
                    Some(tty)
                        if tty.flags.contains(TtyFlags::HUNG_UP)
                            || !tty.driver.output_pending() =>
                    {
                        break;
                    }
                    None => break,
                    _ => {}
                }
            }
            core::hint::spin_loop();
        }
    }

    Ok(())
}

/// Internal helper that applies termios changes with optional drain and
/// input-flush semantics.
///
/// # Background write protection on `tcsetattr`
///
/// Before applying any termios change, the function checks whether the
/// calling process is in the foreground group of the target TTY.  Per
/// POSIX, a background process that calls `tcsetattr` receives `SIGTTOU`
/// unless the signal is blocked or set to `SIG_IGN`.  If the background
/// process group is orphaned, `EIO` is returned instead (there is no
/// parent to continue a stopped group).
pub(super) fn set_termios_mode(
    idx: TtyIndex,
    t: &UserTermios,
    mode: TermiosSetMode,
) -> Result<(), TtyError> {
    // c_cflag CBAUD bits are authoritative for baud rate.
    // c_ispeed/c_ospeed are informational fields populated by get_termios()
    // but do NOT override c_cflag on set_termios().  This matches existing
    // POSIX semantics and the test_review_speed_fields_merge_into_cflag
    // contract.
    let merged = *t;

    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Post-hangup I/O hardening — state-changing ioctls
    // on a hung-up TTY return EIO.
    {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.flags.contains(TtyFlags::HUNG_UP) {
                return Err(TtyError::HungUp);
            }
        }
    }

    // Background write protection — SIGTTOU on tcsetattr.
    // Only enforce for real tasks (task_id != 0 avoids early-boot writes).
    let task_id = current_task_id();
    if task_id != 0 {
        let caller_pgid = current_task_pgid();
        let caller_sid = current_task_sid();

        let check_result = {
            let guard = TTY_SLOTS[slot].lock();
            match guard.as_ref() {
                Some(tty) => {
                    // tcsetattr always enforces foreground check (unlike
                    // write() which only checks when TOSTOP is set).
                    tty.session.check_write(caller_pgid, caller_sid, true)
                }
                None => return Err(TtyError::NotAllocated),
            }
        };

        match check_result {
            ForegroundCheck::BackgroundWrite => {
                // POSIX §11.1.4: tcsetattr() always triggers SIGTTOU for
                // background processes, regardless of TOSTOP.  This differs
                // from write(), which only checks when TOSTOP is set.
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

    // Drain pending output if required by the mode.
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

/// Set termios for a specific TTY.
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

/// Returns `true` if all output to the given TTY has been fully drained:
/// no in-flight writes and no driver-level pending output.
///
/// Exposed for test observability and `TIOCOUTQ`.  Production
/// callers that need to *block* until drain completes should use
/// `wait_output_idle()` (via `TCSETSW` / `TCSETSF` / `tcsbrk(arg>0)`).
///
/// A hung-up TTY is considered idle (drain is vacuously
/// complete because hangup discards all pending output).
#[must_use]
pub fn is_output_idle(idx: TtyIndex) -> Result<bool, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    if TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire) != 0 {
        return Ok(false);
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => Ok(tty.flags.contains(TtyFlags::HUNG_UP) || !tty.driver.output_pending()),
        None => Err(TtyError::NotAllocated),
    }
}

// ---------------------------------------------------------------------------
// Line discipline get / set
// ---------------------------------------------------------------------------

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

/// Switch the line discipline for a TTY.
///
/// # Safety invariant — old ldisc preserved on failure
///
/// If `ldisc_id` is unsupported, this returns `Err(UnsupportedLineDiscipline)`
/// **without touching the current ldisc** — no flush, no state change.  The
/// TTY continues operating with its previous line discipline.  This is safe
/// because `LdiscKind::from_id()` constructs the new ldisc *before* any
/// mutation of the old one.  Only after successful construction does the
/// old ldisc get flushed and replaced.
///
/// All ldisc access in SlopOS goes through the per-slot `SpinLock`, so
/// there is no risk of concurrent reads observing a half-switched ldisc.
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

        // Post-hangup I/O hardening — state-changing ioctls
        // on a hung-up TTY return EIO.
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

    // ldisc switch flush → TIOCPKT_FLUSHREAD (after lock released).
    if did_flush {
        pty::queue_packet_event(idx, slopos_abi::syscall::TIOCPKT_FLUSHREAD);
    }

    result
}

// ---------------------------------------------------------------------------
// Window size
// ---------------------------------------------------------------------------

/// Get window size for a specific TTY.
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

/// Set window size for a specific TTY.
///
/// If the new size differs from the old size, sends SIGWINCH to the
/// foreground process group so applications can re-query dimensions.
#[must_use]
pub fn set_winsize(idx: TtyIndex, ws: &UserWinsize) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Post-hangup I/O hardening — state-changing ioctls
    // on a hung-up TTY return EIO.
    {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.flags.contains(TtyFlags::HUNG_UP) {
                return Err(TtyError::HungUp);
            }
        }
    }

    // The window size is a property of the terminal PAIR: setting it on
    // either PTY end updates both views (Linux keeps one struct for the
    // pair), and SIGWINCH targets the SLAVE side's foreground job — the
    // session lives on the slave, where the shell did TIOCSCTTY.
    let mut deferred = PostLockWork::new();
    // Pinned peer backing (keeps the peer slot alive across the second
    // lock below) plus whether the *target* of the ioctl was the master.
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
                        // Slave targeted directly: its own session carries
                        // the foreground job.
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

// ---------------------------------------------------------------------------
// Missing ioctls (TCFLSH, TCSBRK, TCXONC)
// ---------------------------------------------------------------------------

/// Flush TTY queues (implements `tcflush()` / `TCFLSH` ioctl).
///
/// `queue` values:
///   - `TCIFLUSH` (0): flush input (edit + cooked buffers).  Also clears
///     the PTY throttle flag (matching Linux's `n_tty_flush_buffer()` →
///     `tty_unthrottle()` pattern) so a blocked master writer is woken.
///   - `TCOFLUSH` (1): flush output (reset inflight counter).  Note: this
///     only resets the in-flight tracking counter — it does not flush any
///     driver-level hardware TX FIFO.  For synchronous backends (serial,
///     vconsole) this is a no-op since output completes inline.  Future
///     async/interrupt-driven drivers must implement `flush_output()` on
///     `TtyDriverKind` and call it here.
///   - `TCIOFLUSH` (2): flush both.
pub fn tcflush(idx: TtyIndex, queue: i32) -> Result<(), TtyError> {
    use slopos_abi::syscall::{TCIFLUSH, TCIOFLUSH, TCOFLUSH};
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Peer to wake after unthrottling (pinned inside the lock, wake
    // delivered outside to respect lock ordering).
    let mut unthrottled_peer = None;

    match queue {
        TCIFLUSH | TCIOFLUSH => {
            {
                let mut guard = TTY_SLOTS[slot].lock();
                if let Some(tty) = guard.as_mut() {
                    tty.ldisc.flush_input();

                    // Linux: n_tty_flush_buffer() calls tty_unthrottle().
                    // After flushing all input the buffer is empty, so the
                    // throttle condition is no longer true.  Clear it and
                    // resolve the peer to wake.
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
                TTY_OUTPUT_INFLIGHT[slot].store(0, Ordering::Release);
            }
        }
        TCOFLUSH => {
            // See doc comment above — only resets the in-flight counter.
            TTY_OUTPUT_INFLIGHT[slot].store(0, Ordering::Release);
        }
        _ => return Err(TtyError::InvalidArg),
    }

    // Wake the master-side writer now that the lock is released.
    if let Some(master) = unthrottled_peer {
        // Poll waiters park on the output queue too, so one publish covers both.
        BUS.publish(tty_output_event(master.index().0 as usize));
    }

    Ok(())
}

/// Send break / drain output (implements `tcsendbreak()` / `TCSBRK` ioctl).
///
/// # Drain contract
///
/// `arg > 0` delegates to [`wait_output_idle`] — the single authoritative
/// drain path shared with `TCSETSW` / `TCSETSF`.  See its doc comment for
/// the full drain contract.
///
/// # Hangup guard
///
/// Per POSIX, ioctls on a hung-up TTY return `EIO`.  This function checks
/// `hung_up` before attempting drain and returns `Err(HungUp)` early.
///
/// # Arguments
///
/// - `arg == 0`: send break for ~0.25 s — no-op on PTYs and QEMU serial.
/// - `arg > 0`: equivalent to `tcdrain()` — wait for output to complete.
pub fn tcsbrk(idx: TtyIndex, arg: i32) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Hangup guard — state-changing ioctls on a hung-up TTY
    // return EIO (matching Linux and the set_termios_mode pattern).
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
    // arg == 0 is a hardware break — no-op for virtual terminals.
    Ok(())
}

/// Start/stop I/O (implements `tcflow()` / `TCXONC` ioctl).
///
/// Full behavioral implementation replacing the original
/// validation-only stub.
///
/// - `TCOOFF`: suspend output — sets `output_stopped = true` so the write
///   path blocks (or returns EAGAIN for non-blocking FDs).
/// - `TCOON`: resume output — clears `output_stopped`, wakes blocked
///   writers and poll waiters.
/// - `TCIOFF`: transmit the STOP character (`VSTOP`, typically Ctrl+S /
///   XOFF 0x13) to the terminal device.
/// - `TCION`: transmit the START character (`VSTART`, typically Ctrl+Q /
///   XON 0x11) to the terminal device.
///
/// Invalid action codes return `InvalidArg` (matching Linux `EINVAL`).
pub fn tcxonc(idx: TtyIndex, action: i32) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    use slopos_abi::syscall::{TCIOFF, TCION, TCOOFF, TCOON};

    match action {
        TCOOFF => {
            // Suspend output: set output_stopped flag so writers block.
            let was_stopped = {
                let mut guard = TTY_SLOTS[slot].lock();
                let tty = guard.as_mut().ok_or(TtyError::NotAllocated)?;
                let prev = tty.flags.contains(TtyFlags::OUTPUT_STOPPED);
                tty.flags.insert(TtyFlags::OUTPUT_STOPPED);
                prev
            };
            // Notify master of flow-control stop transition.
            if !was_stopped {
                pty::queue_packet_event(idx, slopos_abi::syscall::TIOCPKT_STOP);
            }
            Ok(())
        }
        TCOON => {
            // Resume output: clear output_stopped flag, wake blocked
            // writers and poll waiters.
            let was_stopped = {
                let mut guard = TTY_SLOTS[slot].lock();
                let tty = guard.as_mut().ok_or(TtyError::NotAllocated)?;
                let prev = tty.flags.contains(TtyFlags::OUTPUT_STOPPED);
                tty.flags.remove(TtyFlags::OUTPUT_STOPPED);
                prev
            };
            // Wake writers and poll waiters regardless — no harm in a
            // spurious wake, and it keeps the logic simple.
            BUS.publish(tty_output_event(slot));
            // Notify master of flow-control start transition.
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
            let _write_guard = TTY_WRITE_LOCKS[slot].lock();
            write_driver_unlocked(driver_id, &[stop_byte]);
            Ok(())
        }
        TCION => {
            let (driver_id, start_byte) = {
                let guard = TTY_SLOTS[slot].lock();
                let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
                let start = tty.ldisc.termios().c_cc[CcIndex::Vstart.as_usize()];
                (tty.driver.id(), start)
            };
            let _write_guard = TTY_WRITE_LOCKS[slot].lock();
            write_driver_unlocked(driver_id, &[start_byte]);
            Ok(())
        }
        _ => Err(TtyError::InvalidArg),
    }
}
