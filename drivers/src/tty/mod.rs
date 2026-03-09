//! SlopOS TTY subsystem — per-terminal TTY abstraction.
//!
//! This module replaces the old global singleton TTY with a proper per-terminal
//! architecture modeled after Linux's `tty_struct` + `n_tty` line discipline.
//!
//! # Architecture
//!
//! Each `Tty` instance owns:
//! - A `LineDisc` (line discipline) for input processing
//! - A `TtyDriverKind` (hardware backend — serial or virtual console)
//! - A `TtySession` (session/foreground pgrp + focused task)
//! - A `WaitQueue` for tasks blocked on input
//!
//! The `TTY_SLOTS` array (in `table.rs`) holds up to `MAX_TTYS` terminal
//! instances, each with its own `IrqMutex` for fully independent per-TTY
//! locking (Phase 8).
//!
//! # Public API
//!
//! All public functions take an explicit `TtyIndex` — there are no global
//! shims.  The `TtyServices` function pointers (registered in
//! `syscall_services_init.rs`) perform the `u8 → TtyIndex` conversion at the
//! boundary.

pub mod driver;
pub mod ldisc;
pub mod pty;
pub mod session;
pub mod table;

use core::ffi::c_int;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIGCONT, SIGHUP, SIGTTIN, SIGTTOU, SIGWINCH};
use slopos_abi::syscall::{TOSTOP, UserTermios, UserWinsize};
use slopos_lib::kernel_services::driver_runtime::{
    clear_session_controlling_tty, current_task_id, current_task_pgid, current_task_sid,
    register_idle_wakeup_callback, scheduler_is_enabled, signal_process_group, signal_session,
};

use self::driver::{TtyDriverKind, write_driver_unlocked};
use self::ldisc::{InputAction, LdiscKind, OutputAction};
use self::session::{ForegroundCheck, TtySession};
use self::table::{TTY_INPUT_WAITERS, TTY_SLOTS};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Re-export `TtyIndex` from the ABI crate so that it is the single
/// definition used across the entire kernel.
pub use slopos_abi::syscall::TtyIndex;

/// Maximum number of TTY instances.
pub const MAX_TTYS: usize = 8;

/// The central TTY structure — one per terminal.
pub struct Tty {
    /// Which TTY slot this is (0 = serial console, 1 = virtual console, etc.).
    pub index: TtyIndex,

    /// The line discipline owned by this TTY.
    pub ldisc: LdiscKind,

    /// Hardware driver backend.
    pub driver: TtyDriverKind,

    /// Session/foreground state (includes focused_task_id).
    pub session: TtySession,

    /// Window size (for TIOCGWINSZ / TIOCSWINSZ).
    pub winsize: UserWinsize,

    /// Whether this TTY is active/allocated.
    pub active: bool,

    pub open_count: u32,

    pub hung_up: bool,

    pub peer_closed: bool,
}

/// Kernel-internal error type for TTY operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyError {
    /// TTY index is out of range (>= MAX_TTYS).
    InvalidIndex,
    /// TTY slot is not allocated (None).
    NotAllocated,
    /// Caller is a background process — should receive SIGTTIN.
    BackgroundRead,
    /// Caller is a background process with TOSTOP — should receive SIGTTOU.
    BackgroundWrite,
    /// TTY is hung up — reads return EIO/EOF.
    HungUp,
    /// No data available and O_NONBLOCK is set — EAGAIN.
    WouldBlock,
    /// Permission denied (e.g. different session for TIOCSPGRP).
    PermissionDenied,
    /// Unsupported line discipline ID.
    UnsupportedLineDiscipline,
    /// Caller belongs to a different session than the TTY's controlling
    /// session — hard denial (Phase 19).
    CrossSessionDenied,
}

#[derive(Clone, Copy)]
enum TermiosSetMode {
    Now,
    Drain,
    DrainAndFlushInput,
}

// ---------------------------------------------------------------------------
// Tty helper methods
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
    fn drain_hw_input(&mut self) -> Option<(u32, u8)> {
        let mut scratch = [0u8; 64];
        let count = self.driver.drain_input(&mut scratch);
        let mut deferred_signal = None;

        for i in 0..count {
            let mut c = scratch[i];
            // Serial terminals send CR for Enter and DEL (0x7F) for backspace.
            if c == b'\r' {
                c = b'\n';
            } else if c == 0x7F {
                c = 0x08;
            }

            let action = self.ldisc.input_char(c);
            match action {
                InputAction::Echo { buf, len } => {
                    for j in 0..len as usize {
                        self.driver.write_output(&[buf[j]]);
                    }
                }
                InputAction::Signal(sig) => {
                    deferred_signal = Some((self.session.fg_pgrp_raw(), sig));
                }
                InputAction::ReprintLine => {
                    self.driver.write_output(b"\n");
                    let content = self.ldisc.edit_content();
                    for &b in content {
                        self.driver.write_output(&[b]);
                    }
                }
                InputAction::None => {}
            }
        }

        deferred_signal
    }
}

// ---------------------------------------------------------------------------
// Active TTY tracking (for keyboard input routing)
// ---------------------------------------------------------------------------

/// The currently active TTY index (receives keyboard input).
/// Defaults to 0 (serial console).
static ACTIVE_TTY: AtomicU8 = AtomicU8::new(0);

/// Returns the TTY index that should receive keyboard input.
pub fn active_tty() -> TtyIndex {
    TtyIndex(ACTIVE_TTY.load(Ordering::Relaxed))
}

/// Set the active TTY (the one receiving keyboard input).
pub fn set_active_tty(idx: TtyIndex) {
    ACTIVE_TTY.store(idx.0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Per-TTY public API
// ---------------------------------------------------------------------------

/// Push a raw input byte to a specific TTY.
///
/// Called from interrupt context (keyboard ISR) or from `drain_hw_input`.
/// Feeds the byte through the line discipline and handles echo/signal actions.
pub fn push_input(idx: TtyIndex, c: u8) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let mut route = None;
    let wake = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(t) => t,
            None => return,
        };

        if tty.hung_up {
            return;
        }

        let action = tty.ldisc.input_char(c);
        let has_data = tty.ldisc.has_data();

        // Handle echo, reprint, and signal actions while we hold the lock.
        match action {
            InputAction::Echo { buf, len } => {
                let mut out = [0u8; 1025];
                out[..len as usize].copy_from_slice(&buf[..len as usize]);
                route = Some((tty.driver.id(), out, len as usize));
                has_data
            }
            InputAction::ReprintLine => {
                let mut out = [0u8; 1025];
                out[0] = b'\n';
                let content = tty.ldisc.edit_content();
                let copy_len = core::cmp::min(content.len(), out.len().saturating_sub(1));
                out[1..1 + copy_len].copy_from_slice(&content[..copy_len]);
                route = Some((tty.driver.id(), out, copy_len + 1));
                has_data
            }
            InputAction::Signal(sig) => {
                let pgid = tty.session.fg_pgrp_raw();
                // Release lock before signalling to avoid deadlock.
                drop(guard);
                if pgid != 0 {
                    let _ = signal_process_group(pgid, sig);
                }
                return; // Signal path — wakeup not needed here.
            }
            InputAction::None => has_data,
        }
    };

    if let Some((driver_id, out, out_len)) = route {
        write_driver_unlocked(driver_id, &out[..out_len]);
    }

    if wake {
        notify_input_ready(idx);
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
}

pub use self::pty::{get_pty_number, is_pty_slave, pty_alloc};

/// Read cooked data from a specific TTY.
///
/// Uses `TtySession::check_read()` as the sole read-side gate.  Background
/// processes receive `SIGTTIN` instead of silently blocking.
///
/// Phase 8: drain + foreground check + read are merged into a single per-TTY
/// lock acquisition per loop iteration (previously 5–6 separate locks).
pub fn read(idx: TtyIndex, buf: &mut [u8], nonblock: bool) -> Result<usize, TtyError> {
    read_with_attach(idx, buf, nonblock, true)
}

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
        {
            let mut guard = TTY_SLOTS[slot].lock();
            let tty = match guard.as_mut() {
                Some(t) => t,
                None => return Err(TtyError::NotAllocated),
            };

            // Hung-up check (before drain).
            if tty.peer_closed && !tty.ldisc.has_data() {
                return Ok(0);
            }

            if tty.hung_up && !tty.ldisc.has_data() {
                return if nonblock {
                    Err(TtyError::HungUp)
                } else {
                    Ok(0)
                };
            }

            // Foreground check via check_read().
            if enforce_access {
                match tty.session.check_read(caller_pgid, caller_sid) {
                    ForegroundCheck::BackgroundRead => {
                        drop(guard);
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
            deferred_signal = tty.drain_hw_input();

            // Try to read from the cooked buffer.
            let got = tty.ldisc.read(&mut buf[total..]);
            total = total.saturating_add(got);

            let is_canonical = tty.ldisc.is_canonical();
            let (vmin_u8, vtime_u8) = tty.ldisc.vmin_vtime();
            let vmin = core::cmp::min(vmin_u8 as usize, buf.len());
            let vtime_ms = (vtime_u8 as u64) * 100;

            if is_canonical {
                if total > 0 {
                    // Drop guard before delivering deferred signal.
                    drop(guard);
                    if let Some((pgid, sig)) = deferred_signal {
                        if pgid != 0 {
                            let _ = signal_process_group(pgid, sig);
                        }
                    }
                    return Ok(total);
                }
            } else {
                match (vmin_u8, vtime_u8) {
                    (0, 0) => {
                        drop(guard);
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
                            if let Some((pgid, sig)) = deferred_signal {
                                if pgid != 0 {
                                    let _ = signal_process_group(pgid, sig);
                                }
                            }
                            return Ok(total);
                        }
                        should_wait = true;
                        // Phase 1: no bytes yet — wait indefinitely for
                        // the first byte (timeout = None).
                        // Phase 2: at least one byte received — start the
                        // inter-byte timer for the remaining bytes.
                        if total > 0 {
                            wait_timeout_ms = Some(vtime_ms);
                        }
                        // else: wait_timeout_ms remains None (indefinite)
                    }
                }
            }

            // Check hung-up after drain (data may have been flushed by hangup).
            if tty.peer_closed && !tty.ldisc.has_data() {
                return Ok(0);
            }

            if tty.hung_up {
                return if nonblock {
                    Err(TtyError::HungUp)
                } else {
                    Ok(0)
                };
            }

            if !is_canonical && !should_wait {
                if total > 0 {
                    drop(guard);
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

        // Deliver deferred signal from drain (e.g. Ctrl+C on serial).
        if let Some((pgid, sig)) = deferred_signal {
            if pgid != 0 {
                let _ = signal_process_group(pgid, sig);
            }
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
                        let sig = tty.drain_hw_input();
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
    }
}

/// Write bytes to a specific TTY.
///
/// Applies output processing (`c_oflag`) — e.g. OPOST + ONLCR converts
/// `\n` to `\r\n` before sending to the driver.
///
/// Phase 8: split-write pattern — output is processed through the line
/// discipline under the per-TTY lock into a local stack buffer, the lock is
/// dropped, and the buffered bytes are written to the hardware without
/// holding any TTY lock.  This prevents slow serial I/O from blocking
/// operations on other TTYs.
///
/// Phase 10: write-side foreground check — when `TOSTOP` is set in the
/// TTY's `c_lflag`, background processes receive `SIGTTOU` instead of
/// being silently allowed to write.  This matches POSIX job control.
pub fn write(idx: TtyIndex, data: &[u8]) -> Result<usize, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Phase 10 + Phase 19: Write-side foreground check.
    // Enforce cross-session denial (Phase 19) and TOSTOP (Phase 10).
    // Only enforce for real tasks (task_id != 0 avoids early-boot writes).
    let task_id = current_task_id();
    if task_id != 0 {
        let caller_pgid = current_task_pgid();
        let caller_sid = current_task_sid();
        let guard = TTY_SLOTS[slot].lock();
        let check_result = match guard.as_ref() {
            Some(tty) => {
                let tostop = (tty.ldisc.termios().c_lflag & TOSTOP) != 0;
                Some(tty.session.check_write(caller_pgid, caller_sid, tostop))
            }
            None => return Err(TtyError::NotAllocated),
        };
        drop(guard);

        match check_result {
            Some(ForegroundCheck::BackgroundWrite) => {
                if caller_pgid != 0 {
                    let _ = signal_process_group(caller_pgid, SIGTTOU);
                }
                return Err(TtyError::BackgroundWrite);
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

    let mut pos = 0;
    while pos < data.len() {
        let mut out_buf = [0u8; OUT_BUF_CAP];
        let mut out_len = 0;
        let driver_id;

        // Phase 1: Process output under per-TTY lock (fast — pure computation).
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

        // Phase 2: Driver I/O without any TTY lock (slow — hardware).
        write_driver_unlocked(driver_id, &out_buf[..out_len]);
    }

    Ok(data.len())
}

/// Check if a TTY has cooked data available for reading.
pub fn has_data(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        let _ = tty.drain_hw_input();
        tty.ldisc.has_data()
    } else {
        false
    }
}

/// Get termios for a specific TTY.
pub fn get_termios(idx: TtyIndex) -> Result<UserTermios, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => Ok(*tty.ldisc.termios()),
        None => Err(TtyError::NotAllocated),
    }
}

fn wait_output_idle(idx: TtyIndex) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(_) => Ok(()),
        None => Err(TtyError::NotAllocated),
    }
}

fn set_termios_mode(idx: TtyIndex, t: &UserTermios, mode: TermiosSetMode) -> Result<(), TtyError> {
    if matches!(
        mode,
        TermiosSetMode::Drain | TermiosSetMode::DrainAndFlushInput
    ) {
        wait_output_idle(idx)?;
    }

    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let mut guard = TTY_SLOTS[slot].lock();
    match guard.as_mut() {
        Some(tty) => {
            if matches!(mode, TermiosSetMode::DrainAndFlushInput) {
                tty.ldisc.flush_input();
            }
            tty.ldisc.set_termios(t);
            tty.driver.set_termios(t);
            Ok(())
        }
        None => Err(TtyError::NotAllocated),
    }
}

/// Set termios for a specific TTY.
pub fn set_termios(idx: TtyIndex, t: &UserTermios) -> Result<(), TtyError> {
    set_termios_mode(idx, t, TermiosSetMode::Now)
}

pub fn set_termios_wait(idx: TtyIndex, t: &UserTermios) -> Result<(), TtyError> {
    set_termios_mode(idx, t, TermiosSetMode::Drain)
}

pub fn set_termios_flush(idx: TtyIndex, t: &UserTermios) -> Result<(), TtyError> {
    set_termios_mode(idx, t, TermiosSetMode::DrainAndFlushInput)
}

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

pub fn set_ldisc(idx: TtyIndex, ldisc_id: u32) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let mut guard = TTY_SLOTS[slot].lock();
    let tty = match guard.as_mut() {
        Some(tty) => tty,
        None => return Err(TtyError::NotAllocated),
    };

    if tty.ldisc.id() == ldisc_id {
        let mut termios = *tty.ldisc.termios();
        termios.c_line = ldisc_id as u8;
        tty.ldisc.set_termios(&termios);
        tty.driver.set_termios(tty.ldisc.termios());
        return Ok(());
    }

    let mut termios = *tty.ldisc.termios();
    termios.c_line = ldisc_id as u8;
    let Some(new_ldisc) = LdiscKind::from_id(ldisc_id, termios) else {
        return Err(TtyError::UnsupportedLineDiscipline);
    };

    tty.ldisc.flush_input();
    tty.ldisc = new_ldisc;
    tty.driver.set_termios(tty.ldisc.termios());
    Ok(())
}

/// Get the foreground process group for a specific TTY.
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
/// change the foreground pgrp.  Returns 0 on success, -1 on permission error.
pub fn set_foreground_pgrp_checked(
    idx: TtyIndex,
    pgid: u32,
    caller_sid: u32,
) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
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

/// Get window size for a specific TTY.
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
pub fn set_winsize(idx: TtyIndex, ws: &UserWinsize) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    let signal_pgid = {
        let mut guard = TTY_SLOTS[slot].lock();
        match guard.as_mut() {
            Some(tty) => {
                let old = tty.winsize;
                tty.winsize = *ws;
                // Only signal if dimensions actually changed.
                if old.ws_row != ws.ws_row || old.ws_col != ws.ws_col {
                    let pgid = tty.session.fg_pgrp_raw();
                    if pgid != 0 { Some(pgid) } else { None }
                } else {
                    None
                }
            }
            None => return Err(TtyError::NotAllocated),
        }
    };

    // Deliver SIGWINCH outside the lock to avoid deadlock.
    if let Some(pgid) = signal_pgid {
        let _ = signal_process_group(pgid, SIGWINCH);
    }

    Ok(())
}

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
pub fn set_compositor_focus(task_id: u32) -> Result<(), TtyError> {
    let idx = active_tty();
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
pub fn get_compositor_focus() -> Result<u32, TtyError> {
    let idx = active_tty();
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

/// Initialise the TTY subsystem.  Call during early boot after serial is ready.
pub fn init() {
    table::tty_table_init();
}

// ---------------------------------------------------------------------------
// Session management API
// ---------------------------------------------------------------------------

/// Get the session ID for a specific TTY.
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

/// Re-export `detach_session_by_id` from `session.rs` (Phase 14 extraction).
pub use self::session::detach_session_by_id;

pub fn open_ref(idx: TtyIndex) -> Result<u32, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        let peer_to_reopen = match tty.driver {
            TtyDriverKind::PtySlave { master_idx } => Some(master_idx),
            _ => None,
        };
        tty.open_count = tty
            .open_count
            .checked_add(1)
            .unwrap_or_else(|| panic!("tty open_count overflow for idx {}", idx.0));
        tty.hung_up = false;
        tty.peer_closed = false;
        let open_count = tty.open_count;
        drop(guard);

        if let Some(peer_idx) = peer_to_reopen {
            pty::clear_peer_closed(peer_idx);
        }

        return Ok(open_count);
    }
    Err(TtyError::NotAllocated)
}

pub fn close_ref(idx: TtyIndex) -> Result<u32, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        if tty.open_count == 0 {
            return Ok(0);
        }
        tty.open_count -= 1;
        let open_count = tty.open_count;
        if tty.open_count == 0 {
            match tty.driver {
                TtyDriverKind::PtyMaster { slave_idx } => {
                    drop(guard);
                    hangup(slave_idx);
                    pty::free_pair_if_unused(idx, slave_idx);
                    return Ok(0);
                }
                TtyDriverKind::PtySlave { master_idx } => {
                    drop(guard);
                    pty::mark_peer_closed(master_idx);
                    pty::free_pair_if_unused(idx, master_idx);
                    return Ok(0);
                }
                TtyDriverKind::SerialConsole(_)
                | TtyDriverKind::VConsole(_)
                | TtyDriverKind::None => {
                    tty.ldisc.flush_all();
                    tty.session.detach();
                    tty.hung_up = false;
                    tty.peer_closed = false;
                }
            }
        }
        return Ok(open_count);
    }
    Err(TtyError::NotAllocated)
}

pub fn hangup(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let session_id = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(t) => t,
            None => return,
        };
        let sid = tty.session.session_id_raw();
        tty.ldisc.flush_all();
        tty.session.detach();
        tty.hung_up = true;
        sid
    };

    // Phase 15: Signal the entire session (not just fg_pgrp) so that all
    // processes in the session receive SIGHUP + SIGCONT on hangup.
    if session_id != 0 {
        let _ = clear_session_controlling_tty(session_id, idx);
        let _ = signal_session(session_id, SIGHUP);
        let _ = signal_session(session_id, SIGCONT);
    }

    if scheduler_is_enabled() != 0 {
        TTY_INPUT_WAITERS[slot].wake_all();
    }
}

pub fn is_hung_up(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => tty.hung_up,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Idle callback (Phase 8: iterates ALL active TTYs)
// ---------------------------------------------------------------------------

/// Idle-loop callback: drain hardware input and wake blocked readers.
///
/// Phase 8: now iterates all active TTYs instead of only TTY 0.  Each
/// per-TTY lock is acquired and released individually.
fn input_available_cb() -> c_int {
    let mut any_data = false;
    for i in 0..MAX_TTYS {
        let has_data = {
            let mut guard = TTY_SLOTS[i].lock();
            if let Some(tty) = guard.as_mut() {
                if tty.active {
                    let _ = tty.drain_hw_input();
                    tty.ldisc.has_data()
                } else {
                    false
                }
            } else {
                false
            }
        };
        if has_data {
            notify_input_ready(TtyIndex(i as u8));
            any_data = true;
        }
    }
    any_data as c_int
}

fn register_idle_callback() {
    use core::sync::atomic::{AtomicBool, Ordering};
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    register_idle_wakeup_callback(Some(input_available_cb));
}
