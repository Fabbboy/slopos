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
//!
//! # Locking Convention (Phase 29)
//!
//! Methods that operate on a `Tty` while the slot `IrqMutex` is already held
//! use the `*_locked()` suffix (e.g. `drain_hw_input_locked`).  This makes the
//! caller responsible for acquiring the lock and documents the precondition at
//! the call site.

pub mod driver;
pub mod ldisc;
pub mod pty;
pub mod ringbuf;
pub mod session;
pub mod table;
pub mod vconsole;
pub mod vtparser;

use core::ffi::c_int;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIGCONT, SIGHUP, SIGTTIN, SIGTTOU, SIGWINCH};
use slopos_abi::syscall::{LocalFlags, UserTermios, UserWinsize};
use slopos_lib::kernel_services::driver_runtime::{
    clear_session_controlling_tty, current_task_id, current_task_pgid, current_task_sid,
    has_pending_signal, is_current_signal_blocked_or_ignored, is_pgrp_orphaned,
    register_idle_wakeup_callback, scheduler_is_enabled, signal_process_group, signal_session,
};

use self::driver::{TtyDriverKind, write_driver_unlocked};
use self::ldisc::{InputAction, LdiscKind, OutputAction};
use self::session::{ForegroundCheck, TtySession};
use self::table::{
    TTY_INPUT_WAITERS, TTY_OUTPUT_INFLIGHT, TTY_OUTPUT_WAITERS, TTY_POLL_WAITERS, TTY_SLOTS,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Re-export `TtyIndex` from the ABI crate so that it is the single
/// definition used across the entire kernel.
pub use slopos_abi::syscall::TtyIndex;

/// Maximum number of TTY instances.
pub const MAX_TTYS: usize = 32;

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

    /// Phase 38: PTY slave lock state.  When `true`, the corresponding
    /// `/dev/pts/N` device node cannot be opened.  Only meaningful for
    /// PTY slaves (always `false` for consoles and masters).  Defaults to
    /// `true` on `pty_alloc()` — the master holder must unlock via
    /// `TIOCSPTLCK` before the slave can be opened.
    pub slave_locked: bool,

    /// Phase 39: PTY packet mode.  When `true` on a PTY master, every
    /// `read()` is prefixed with a single control byte indicating the
    /// event type (see `TIOCPKT_*` constants in `abi`).
    pub packet_mode: bool,

    /// Phase 39: Pending packet-mode event bits.  Bitwise OR of
    /// `TIOCPKT_FLUSHREAD`, `TIOCPKT_FLUSHWRITE`, `TIOCPKT_STOP`,
    /// `TIOCPKT_START`, `TIOCPKT_NOSTOP`, `TIOCPKT_DOSTOP`.
    /// Consumed on the next master `read()` when non-zero.
    pub packet_events: u8,

    /// Finishing Phase 2: PTY flow control throttle flag.  When `true`,
    /// the slave's cooked buffer has exceeded `THROTTLE_HIGH_WATER` and
    /// the master-side writer must be back-pressured (blocked or EAGAIN).
    /// Cleared when a slave `read()` drains below `THROTTLE_LOW_WATER`.
    pub throttled: bool,

    /// Finishing Phase 8: Explicit output-stop state for TCXONC.  When `true`,
    /// `tty_write()` blocks (or returns EAGAIN for non-blocking) until output
    /// is resumed via `tcxonc(TCOON)`.  Separate from the ldisc `stopped`
    /// flag which is driven by IXON (Ctrl+S / Ctrl+Q keyboard flow control).
    pub output_stopped: bool,
}

/// Kernel-internal error type for TTY operations.
///
/// # Phase 28: `to_errno()` boundary mapping
///
/// Each variant maps to a POSIX errno at the syscall boundary via
/// [`TtyError::to_errno()`].  Internal code matches on variants directly;
/// the adapter layer in `syscall_services_init.rs` calls `to_errno()`.
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
    /// Operation was interrupted by a signal (Phase 28).
    SignalInterrupt,
    /// Background process in an orphaned process group tried to change
    /// terminal settings — returns EIO instead of SIGTTOU (Phase 31).
    OrphanedProcessGroup,
    /// Invalid argument (Finishing Phase 5: bad `tcflush` queue selector).
    InvalidArg,
    /// Finishing Phase 7: Blocking syscall was interrupted by a signal and
    /// may be transparently restarted.  Maps to the kernel-internal
    /// ERESTARTSYS (-512) — the syscall return path converts this to EINTR
    /// or restarts depending on SA_RESTART.  MUST NEVER reach userland.
    Restart,
}

impl TtyError {
    /// Map this error to a negative errno value for the syscall boundary.
    #[inline]
    pub const fn to_errno(self) -> i32 {
        use slopos_abi::syscall::*;
        match self {
            TtyError::InvalidIndex => ERRNO_EINVAL as i32,
            TtyError::NotAllocated => ERRNO_ENXIO as i32,
            TtyError::BackgroundRead => ERRNO_EIO as i32,
            TtyError::BackgroundWrite => ERRNO_EIO as i32,
            TtyError::HungUp => ERRNO_EIO as i32,
            TtyError::WouldBlock => ERRNO_EAGAIN as i32,
            TtyError::PermissionDenied => ERRNO_EPERM as i32,
            TtyError::UnsupportedLineDiscipline => ERRNO_EINVAL as i32,
            TtyError::CrossSessionDenied => ERRNO_EIO as i32,
            TtyError::SignalInterrupt => ERRNO_EINTR as i32,
            TtyError::OrphanedProcessGroup => ERRNO_EIO as i32,
            TtyError::InvalidArg => ERRNO_EINVAL as i32,
            TtyError::Restart => ERRNO_ERESTARTSYS as i32,
        }
    }
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
    fn drain_hw_input_locked(&mut self) -> Option<(u32, u8)> {
        let mut scratch = [0u8; 64];
        let count = self.driver.drain_input(&mut scratch);
        let mut deferred_signal = None;
        let mut echo_buf = [0u8; 256];
        let mut echo_len = 0usize;

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
                    let n = len as usize;
                    if echo_len + n <= echo_buf.len() {
                        echo_buf[echo_len..echo_len + n].copy_from_slice(&buf[..n]);
                        echo_len += n;
                    } else {
                        if echo_len > 0 {
                            self.driver.write_output(&echo_buf[..echo_len]);
                            echo_len = 0;
                        }
                        self.driver.write_output(&buf[..n]);
                    }
                }
                InputAction::Bell => {
                    if echo_len < echo_buf.len() {
                        echo_buf[echo_len] = 0x07;
                        echo_len += 1;
                    } else {
                        if echo_len > 0 {
                            self.driver.write_output(&echo_buf[..echo_len]);
                            echo_len = 0;
                        }
                        self.driver.write_output(&[0x07]);
                    }
                }
                InputAction::Signal(sig) => {
                    if echo_len > 0 {
                        self.driver.write_output(&echo_buf[..echo_len]);
                        echo_len = 0;
                    }
                    deferred_signal = Some((self.session.fg_pgrp_raw(), sig));
                }
                InputAction::ReprintLine => {
                    if echo_len > 0 {
                        self.driver.write_output(&echo_buf[..echo_len]);
                        echo_len = 0;
                    }
                    self.driver.write_output(b"\n");
                    let content = self.ldisc.edit_content();
                    for &b in content {
                        self.driver.write_output(&[b]);
                    }
                }
                InputAction::KillLineEcho { columns } => {
                    if echo_len > 0 {
                        self.driver.write_output(&echo_buf[..echo_len]);
                        echo_len = 0;
                    }
                    for _ in 0..columns {
                        self.driver.write_output(&[0x08, 0x20, 0x08]);
                    }
                }
                InputAction::None => {}
            }

            // Phase 36: IXOFF — send XOFF if input buffer is filling up.
            if let Some(xoff) = self.ldisc.ixoff_check_xoff() {
                self.driver.write_output(&[xoff]);
            }
        }

        if echo_len > 0 {
            self.driver.write_output(&echo_buf[..echo_len]);
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
static DEFAULT_CONSOLE_TTY: AtomicU8 = AtomicU8::new(0);

/// Returns the TTY index that should receive keyboard input.
pub fn active_tty() -> TtyIndex {
    TtyIndex(ACTIVE_TTY.load(Ordering::Relaxed))
}

/// Set the active TTY (the one receiving keyboard input).
pub fn set_active_tty(idx: TtyIndex) {
    ACTIVE_TTY.store(idx.0, Ordering::Relaxed);
}

/// Switch keyboard routing to a specific active TTY.
///
/// This controls only the TTY input route (`active_tty`). It does not alter:
/// - compositor focus (UI/window focus)
/// - POSIX foreground process group/job control (`fg_pgrp`)
#[must_use]
pub fn switch_active_tty(idx: TtyIndex) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(tty) if tty.active => {}
            _ => return Err(TtyError::NotAllocated),
        }
    }

    set_active_tty(idx);
    if scheduler_is_enabled() != 0 {
        TTY_INPUT_WAITERS[slot].wake_all();
        TTY_POLL_WAITERS[slot].wake_all();
    }
    Ok(())
}

pub fn set_default_console_tty(idx: TtyIndex) {
    DEFAULT_CONSOLE_TTY.store(idx.0, Ordering::Relaxed);
}

pub fn default_console_tty() -> TtyIndex {
    TtyIndex(DEFAULT_CONSOLE_TTY.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Per-TTY public API
// ---------------------------------------------------------------------------

/// Push a raw input byte to a specific TTY.
///
/// Called from interrupt context (keyboard ISR) or from `drain_hw_input_locked`.
/// Feeds the byte through the line discipline and handles echo/signal actions.
pub fn push_input(idx: TtyIndex, c: u8) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let mut route = None;
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

        // Phase 21: Track stopped state before input processing so we
        // can detect stopped→resumed transitions for IXON wakeup.
        let was_stopped = tty.ldisc.is_stopped();

        let action = tty.ldisc.input_char(c);
        if let Some(xoff) = tty.ldisc.ixoff_check_xoff() {
            ixoff_byte_out = Some((tty.driver.id(), xoff));
        }

        // Phase 21: If output transitioned from stopped to resumed,
        // wake blocked writers and poll waiters.
        if was_stopped && !tty.ldisc.is_stopped() {
            output_resumed = true;
        }

        // Phase 39: Track flow-control transitions for packet mode.
        // Deferred: queue_packet_event acquires its own TTY slot lock,
        // so we must not call it while holding `guard`.
        if !was_stopped && tty.ldisc.is_stopped() {
            deferred_pkt_event = slopos_abi::syscall::TIOCPKT_STOP;
        } else if was_stopped && !tty.ldisc.is_stopped() {
            deferred_pkt_event = slopos_abi::syscall::TIOCPKT_START;
        }

        // Finishing Phase 2: Activate throttle when cooked buffer fills.
        if !tty.throttled && tty.ldisc.bytes_available() >= ldisc::THROTTLE_HIGH_WATER {
            tty.throttled = true;
        }

        // Handle echo, reprint, and signal actions while we hold the lock.
        match action {
            InputAction::Echo { buf, len } => {
                let mut out = [0u8; 1025];
                out[..len as usize].copy_from_slice(&buf[..len as usize]);
                route = Some((tty.driver.id(), out, len as usize));
            }
            InputAction::ReprintLine => {
                let mut out = [0u8; 1025];
                out[0] = b'\n';
                let content = tty.ldisc.edit_content();
                let copy_len = core::cmp::min(content.len(), out.len().saturating_sub(1));
                out[1..1 + copy_len].copy_from_slice(&content[..copy_len]);
                route = Some((tty.driver.id(), out, copy_len + 1));
            }
            InputAction::KillLineEcho { columns } => {
                // Phase 27: Build BS-SP-BS triples for visual line erase.
                let mut out = [0u8; 1025];
                let triples = core::cmp::min(columns as usize, out.len() / 3);
                for i in 0..triples {
                    out[i * 3] = 0x08;
                    out[i * 3 + 1] = 0x20;
                    out[i * 3 + 2] = 0x08;
                }
                route = Some((tty.driver.id(), out, triples * 3));
            }
            InputAction::Signal(sig) => {
                let pgid = tty.session.fg_pgrp_raw();
                // Release lock before signalling to avoid deadlock.
                drop(guard);
                if let Some((driver_id, xoff)) = ixoff_byte_out {
                    write_driver_unlocked(driver_id, &[xoff]);
                }
                if pgid != 0 {
                    let _ = signal_process_group(pgid, sig);
                }
                // Even on signal path, wake output waiters if resumed.
                if output_resumed {
                    TTY_OUTPUT_WAITERS[slot].wake_all();
                    TTY_POLL_WAITERS[slot].wake_all();
                }
                // Phase 39: Deliver deferred packet event.
                if deferred_pkt_event != 0 {
                    pty::queue_packet_event(idx, deferred_pkt_event);
                }
                return;
            }
            InputAction::Bell => {
                let mut out = [0u8; 1025];
                out[0] = 0x07;
                route = Some((tty.driver.id(), out, 1));
            }
            InputAction::None => {}
        }

        // Finishing Phase 10: Use batched wake policy instead of waking
        // on every byte.  `should_wake_reader()` coalesces non-canonical
        // mode wakeups behind the WAKEUP_CHARS threshold while preserving
        // immediate wake on canonical line boundaries.
        tty.ldisc.should_wake_reader()
    };

    // Phase 39: Deliver deferred packet event now that the slot lock is released.
    if deferred_pkt_event != 0 {
        pty::queue_packet_event(idx, deferred_pkt_event);
    }

    if let Some((driver_id, out, out_len)) = route {
        // Phase 25: Track in-flight echo output for drain semantics.
        TTY_OUTPUT_INFLIGHT[slot].fetch_add(1, Ordering::Release);
        write_driver_unlocked(driver_id, &out[..out_len]);
        TTY_OUTPUT_INFLIGHT[slot].fetch_sub(1, Ordering::Release);
        TTY_OUTPUT_WAITERS[slot].wake_all();
    }

    // Phase 36: IXOFF — send XOFF byte to terminal if high-water exceeded.
    if let Some((driver_id, xoff)) = ixoff_byte_out {
        write_driver_unlocked(driver_id, &[xoff]);
    }

    if wake {
        notify_input_ready(idx);
    }

    // Phase 21: Wake blocked writers and poll waiters on IXON resume.
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
    // Phase 21 + Finishing Phase 1: Wake per-slot poll sleepers.
    TTY_POLL_WAITERS[slot].wake_all();
}

pub use self::pty::{
    get_packet_mode, get_pty_lock, get_pty_number, is_pty_slave, is_slave_locked, pty_alloc,
    pty_open_slave, queue_packet_event, set_packet_mode, set_pty_lock,
};

/// Read cooked data from a specific TTY.
///
/// Uses `TtySession::check_read()` as the sole read-side gate.  Background
/// processes receive `SIGTTIN` instead of silently blocking.
///
/// Phase 8: drain + foreground check + read are merged into a single per-TTY
/// lock acquisition per loop iteration (previously 5–6 separate locks).
#[must_use]
pub fn read(idx: TtyIndex, buf: &mut [u8], nonblock: bool) -> Result<usize, TtyError> {
    read_with_attach(idx, buf, nonblock, true)
}

/// Phase 23 note: `_auto_attach` is intentionally dead.  Phase 18 removed
/// durable read-side ownership mutation, so reads no longer claim controlling
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
        // Finishing Phase 2: Track if this read unthrottled the TTY so we
        // can wake the master-side writer after releasing the lock.
        let mut unthrottled_peer: Option<usize> = None;
        // Finishing Phase 12: Track if no_room recovery happened so we
        // can wake producers after releasing the lock.
        let mut no_room_recovered = false;
        {
            let mut guard = TTY_SLOTS[slot].lock();
            let tty = match guard.as_mut() {
                Some(t) => t,
                None => return Err(TtyError::NotAllocated),
            };

            // Phase 33: Post-hangup I/O hardening — a hung-up TTY is
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

            // Phase 39: PTY packet mode — if this master has pending
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
                        // Finishing Phase 2: Unthrottle after packet-mode read.
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
                        // Finishing Phase 12: No-room recovery after packet-mode drain.
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
                        // Finishing Phase 2: Wake master-side writer after unthrottle.
                        if let Some(ps) = pkt_unthrottled_peer {
                            if ps < MAX_TTYS {
                                TTY_OUTPUT_WAITERS[ps].wake_all();
                                TTY_POLL_WAITERS[ps].wake_all();
                            }
                        }
                        // Finishing Phase 12: Wake local waiters on no_room recovery.
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
                // Finishing Phase 2: Unthrottle if occupancy dropped below low-water.
                if got > 0
                    && tty.throttled
                    && tty.ldisc.bytes_available() <= ldisc::THROTTLE_LOW_WATER
                {
                    tty.throttled = false;
                    if let TtyDriverKind::PtySlave { peer } = &tty.driver {
                        unthrottled_peer = Some(peer.idx.0 as usize);
                    }
                }
                // Finishing Phase 12: No-room recovery after normal drain.
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
                    // Finishing Phase 2: Wake master after unthrottle.
                    if let Some(ps) = unthrottled_peer {
                        if ps < MAX_TTYS {
                            TTY_OUTPUT_WAITERS[ps].wake_all();
                            TTY_POLL_WAITERS[ps].wake_all();
                        }
                    }
                    // Finishing Phase 12: Wake local waiters on no_room recovery.
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

            // Phase 33: Check hung-up after drain (data may have been
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

        // Finishing Phase 2: Wake master-side writer if this read unthrottled.
        if let Some(ps) = unthrottled_peer {
            if ps < MAX_TTYS {
                TTY_OUTPUT_WAITERS[ps].wake_all();
                TTY_POLL_WAITERS[ps].wake_all();
            }
        }

        // Finishing Phase 12: Wake local waiters on no_room recovery.
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
            // Finishing Phase 7: Check for pending signals so the wait
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

        // Finishing Phase 7: If the wait was broken by a pending signal
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
///
/// Phase 31: TOSTOP audit — added SIGTTOU blocked/ignored bypass and
/// orphaned process group → EIO handling to match `tcsetattr` semantics.
#[must_use]
pub fn write(idx: TtyIndex, data: &[u8], nonblock: bool) -> Result<usize, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Phase 33: Post-hangup I/O hardening — writes to a hung-up TTY
    // always return EIO.  The data has nowhere to go.
    {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.hung_up {
                return Err(TtyError::HungUp);
            }
        }
    }

    // Phase 10 + Phase 19 + Phase 31: Write-side foreground check.
    // Enforce cross-session denial (Phase 19) and TOSTOP (Phase 10).
    // Phase 31: bypass if SIGTTOU is blocked/ignored; return EIO for
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
                // Phase 31: if SIGTTOU is blocked or ignored, proceed silently.
                if !is_current_signal_blocked_or_ignored(SIGTTOU) {
                    // Phase 31: orphaned pgrp → EIO.
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

    // Finishing Phase 2: Determine if this TTY is a PTY master so we can
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
        // Finishing Phase 2: PTY master throttle back-pressure.
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
                    let guard = TTY_SLOTS[peer_slot].lock();
                    match guard.as_ref() {
                        Some(tty) => !tty.throttled || tty.hung_up || tty.peer_closed,
                        None => true,
                    }
                });
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

        // Phase 21 + Finishing Phase 8: Output-stop enforcement.
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
                let guard = TTY_SLOTS[slot].lock();
                match guard.as_ref() {
                    Some(tty) => !tty.ldisc.is_stopped() && !tty.output_stopped,
                    None => true, // slot gone — let the next lock attempt return NotAllocated
                }
            });
        }

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
        // Phase 25: Track in-flight output for drain semantics.
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

/// Check if a TTY has cooked data available for reading.
///
/// Phase 23: Properly captures and delivers deferred signals from
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

/// Phase 27: Get the number of bytes available for reading from a TTY.
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

/// Finishing Phase 9: Get the number of bytes queued for output on a TTY.
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
            // Phase 13: use output_pending_bytes() for finer-grained
            // queue depth reporting (defaults to 0/1 for bool-only drivers).
            tty.driver.output_pending_bytes()
        } else {
            return Err(TtyError::NotAllocated);
        }
    };
    Ok(inflight + driver_pending)
}

/// Finishing Phase 4: Map baud rate bits from `c_cflag & CBAUD` to numeric speed.
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

/// Get termios for a specific TTY.
///
/// Finishing Phase 4: Populates `c_ispeed`/`c_ospeed` from the baud rate
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
            let speed = cflag_to_speed(t.c_cflag);
            t.c_ispeed = speed;
            t.c_ospeed = speed;
            Ok(t)
        }
        None => Err(TtyError::NotAllocated),
    }
}

/// Wait until all in-flight output has been transmitted to the hardware.
///
/// # Drain Contract (Phase 13 — Output Drain Semantics Hardening)
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
///   - The TTY is hung up (`tty.hung_up == true`).  Hangup discards all
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
/// genuinely sleep on `TTY_OUTPUT_WAITERS` until the TX FIFO empties.
///
/// ## Scheduler awareness
///
/// The function sleeps on `TTY_OUTPUT_WAITERS[slot]` if the scheduler is
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
            Some(tty) if tty.hung_up => return Ok(()), // hangup discards output
            Some(_) => {}                              // slot alive, proceed
            None => return Err(TtyError::NotAllocated),
        }
    }

    // Fast path: if nothing is in-flight and driver has no pending output,
    // return immediately without touching the wait queue.
    if TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire) == 0 {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.hung_up || !tty.driver.output_pending() {
                return Ok(());
            }
        } else {
            return Ok(()); // slot gone — drain vacuously satisfied
        }
    }

    // Slow path: wait until drain completes.
    if scheduler_is_enabled() != 0 {
        TTY_OUTPUT_WAITERS[slot].wait_event(|| {
            if TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire) != 0 {
                return false;
            }
            let guard = TTY_SLOTS[slot].lock();
            match guard.as_ref() {
                Some(tty) => tty.hung_up || !tty.driver.output_pending(),
                None => true, // slot gone — drain vacuously satisfied
            }
        });
    } else {
        // Pre-scheduler fallback: busy-poll (very early boot only).
        loop {
            if TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire) == 0 {
                let guard = TTY_SLOTS[slot].lock();
                match guard.as_ref() {
                    Some(tty) if tty.hung_up || !tty.driver.output_pending() => break,
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
/// # Phase 31: Background write protection on `tcsetattr`
///
/// Before applying any termios change, the function checks whether the
/// calling process is in the foreground group of the target TTY.  Per
/// POSIX, a background process that calls `tcsetattr` receives `SIGTTOU`
/// unless the signal is blocked or set to `SIG_IGN`.  If the background
/// process group is orphaned, `EIO` is returned instead (there is no
/// parent to continue a stopped group).
fn set_termios_mode(idx: TtyIndex, t: &UserTermios, mode: TermiosSetMode) -> Result<(), TtyError> {
    // c_cflag is the authoritative source for baud rate (POSIX).
    // c_ispeed/c_ospeed are informational fields populated by get_termios;
    // they do not override c_cflag in the standard set_termios path.
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Phase 33: Post-hangup I/O hardening — state-changing ioctls
    // on a hung-up TTY return EIO.
    {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.hung_up {
                return Err(TtyError::HungUp);
            }
        }
    }

    // Phase 31: Background write protection — SIGTTOU on tcsetattr.
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
                // POSIX: if SIGTTOU is blocked or ignored, proceed silently.
                if !is_current_signal_blocked_or_ignored(SIGTTOU) {
                    // Check if the process group is orphaned.
                    if is_pgrp_orphaned(caller_pgid, caller_sid) {
                        return Err(TtyError::OrphanedProcessGroup);
                    }
                    // Deliver SIGTTOU to the caller's process group.
                    if caller_pgid != 0 {
                        let _ = signal_process_group(caller_pgid, SIGTTOU);
                    }
                    return Err(TtyError::SignalInterrupt);
                }
                // SIGTTOU blocked or ignored — fall through to apply termios.
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

    let mut deferred_pkt_events: u8 = 0;
    let result = {
        let mut guard = TTY_SLOTS[slot].lock();
        match guard.as_mut() {
            Some(tty) => {
                // Phase 39: Capture old IXON state before applying termios.
                let old_ixon = tty.ldisc.termios().c_iflag & slopos_abi::syscall::IXON;
                let new_ixon = t.c_iflag & slopos_abi::syscall::IXON;

                if matches!(mode, TermiosSetMode::DrainAndFlushInput) {
                    tty.ldisc.flush_input();
                    // Phase 39: Input flush on slave → TIOCPKT_FLUSHREAD.
                    deferred_pkt_events |= slopos_abi::syscall::TIOCPKT_FLUSHREAD;
                }
                tty.ldisc.set_termios(t);
                tty.driver.set_termios(t);

                // Phase 39: Track IXON toggle for packet mode.
                if old_ixon == 0 && new_ixon != 0 {
                    deferred_pkt_events |= slopos_abi::syscall::TIOCPKT_DOSTOP;
                } else if old_ixon != 0 && new_ixon == 0 {
                    deferred_pkt_events |= slopos_abi::syscall::TIOCPKT_NOSTOP;
                }

                Ok(())
            }
            None => Err(TtyError::NotAllocated),
        }
    };

    // Phase 39: Deliver deferred packet events now that the slot lock is released.
    if deferred_pkt_events != 0 {
        pty::queue_packet_event(idx, deferred_pkt_events);
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
/// Phase 25: Exposed for test observability and `TIOCOUTQ`.  Production
/// callers that need to *block* until drain completes should use
/// `wait_output_idle()` (via `TCSETSW` / `TCSETSF` / `tcsbrk(arg>0)`).
///
/// Phase 13: A hung-up TTY is considered idle (drain is vacuously
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
        Some(tty) => Ok(tty.hung_up || !tty.driver.output_pending()),
        None => Err(TtyError::NotAllocated),
    }
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

        // Phase 33: Post-hangup I/O hardening — state-changing ioctls
        // on a hung-up TTY return EIO.
        if tty.hung_up {
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
        let Some(new_ldisc) = LdiscKind::from_id(ldisc_id, termios) else {
            return Err(TtyError::UnsupportedLineDiscipline);
        };

        tty.ldisc.flush_input();
        did_flush = true;
        tty.ldisc = new_ldisc;
        tty.driver.set_termios(tty.ldisc.termios());
        Ok(())
    };

    // Phase 39: ldisc switch flush → TIOCPKT_FLUSHREAD (after lock released).
    if did_flush {
        pty::queue_packet_event(idx, slopos_abi::syscall::TIOCPKT_FLUSHREAD);
    }

    result
}

/// Get the foreground process group for a specific TTY.
#[must_use]
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
#[must_use]
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
/// change the foreground pgrp.  Phase 24 additionally validates that the
/// target process group actually has living members in the session.
///
/// Returns `Ok(())` on success, `Err(PermissionDenied)` on validation failure.
pub fn set_foreground_pgrp_checked(
    idx: TtyIndex,
    pgid: u32,
    caller_sid: u32,
) -> Result<(), TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }

    // Phase 24: Before acquiring the per-TTY lock, validate that the target
    // pgrp actually exists within the session.  This uses the scheduler's
    // task iterator and must be done outside the TTY lock.  Clearing the
    // foreground group (pgid == 0) is always allowed.
    if pgid != 0 {
        let guard = TTY_SLOTS[slot].lock();
        let session_id = match guard.as_ref() {
            Some(tty) if tty.session.has_session() => tty.session.session_id_raw(),
            Some(_) => 0, // no session attached — skip pgrp validation
            None => return Err(TtyError::NotAllocated),
        };
        drop(guard);

        if session_id != 0 {
            use slopos_lib::kernel_services::driver_runtime::pgrp_exists_in_session;
            if !pgrp_exists_in_session(pgid, session_id) {
                return Err(TtyError::PermissionDenied);
            }
        }
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

    // Phase 33: Post-hangup I/O hardening — state-changing ioctls
    // on a hung-up TTY return EIO.
    {
        let guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_ref() {
            if tty.hung_up {
                return Err(TtyError::HungUp);
            }
        }
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
#[must_use]
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
#[must_use]
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
#[must_use]
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

    if tty.session.session_id_raw() != session_id {
        return Ok(false);
    }

    tty.session.detach();
    Ok(true)
}

/// Phase 24: Detach the calling process from its controlling terminal
/// (TIOCNOTTY semantics).
///
/// If the caller is the session leader, the entire session loses the
/// controlling terminal — the foreground process group receives SIGHUP +
/// SIGCONT (matching POSIX hangup behavior).  The session is detached from
/// the TTY, and `clear_session_controlling_tty` clears every task in the
/// session that still refers to this TTY.
///
/// If the caller is NOT the session leader, only the caller's own
/// `controlling_tty` is cleared (the TTY session state is unaffected).
///
/// Returns `Ok(true)` if the caller was the session leader and signals
/// were sent, `Ok(false)` if only the caller was detached.
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
        // Non-leader: only the caller's controlling_tty field is cleared
        // (done by the ioctl handler).  TTY session state is unchanged.
        return Ok(false);
    }

    // Session leader: detach the session from the TTY and signal the
    // foreground process group.
    let (fg_pgid, session_id) = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(tty) => tty,
            None => return Err(TtyError::NotAllocated),
        };

        // Only the controlling session may detach.
        let tty_sid = tty.session.session_id_raw();
        if tty_sid != 0 && tty_sid != caller_sid {
            return Err(TtyError::PermissionDenied);
        }

        let pgid = tty.session.fg_pgrp_raw();
        let sid = tty.session.session_id_raw();
        tty.session.detach();
        (pgid, sid)
    };

    // Signal delivery OUTSIDE the lock to avoid deadlock.
    if session_id != 0 {
        let _ = clear_session_controlling_tty(session_id, idx);
    }
    if fg_pgid != 0 {
        let _ = signal_process_group(fg_pgid, SIGHUP);
        let _ = signal_process_group(fg_pgid, SIGCONT);
    }

    Ok(true)
}

/// Re-export `detach_session_by_id` from `session.rs` (Phase 14 extraction).
pub use self::session::detach_session_by_id;

#[must_use]
pub fn open_ref(idx: TtyIndex) -> Result<u32, TtyError> {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return Err(TtyError::InvalidIndex);
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        let peer_to_reopen = match tty.driver {
            TtyDriverKind::PtySlave { ref peer } => Some(peer.idx),
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

#[must_use]
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
                TtyDriverKind::PtyMaster { ref peer } => {
                    let slave_idx = peer.idx;
                    drop(guard);
                    hangup(slave_idx);
                    pty::free_pair_if_unused(idx, slave_idx);
                    return Ok(0);
                }
                TtyDriverKind::PtySlave { ref peer } => {
                    let master_idx = peer.idx;
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

// ---------------------------------------------------------------------------
// Finishing Phase 5: Missing ioctls (TCFLSH, TCSBRK, TCXONC)
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

    // Peer slot to wake after unthrottling (resolved inside the lock,
    // wake delivered outside to respect lock ordering).
    let mut unthrottled_peer: Option<usize> = None;

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
                    if tty.throttled {
                        tty.throttled = false;
                        if let TtyDriverKind::PtySlave { ref peer } = tty.driver {
                            unthrottled_peer = Some(peer.idx.0 as usize);
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
    if let Some(peer_slot) = unthrottled_peer {
        if peer_slot < MAX_TTYS {
            TTY_OUTPUT_WAITERS[peer_slot].wake_all();
            TTY_POLL_WAITERS[peer_slot].wake_all();
        }
    }

    Ok(())
}

/// Send break / drain output (implements `tcsendbreak()` / `TCSBRK` ioctl).
///
/// # Drain contract (Phase 13)
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

    // Phase 13: Hangup guard — state-changing ioctls on a hung-up TTY
    // return EIO (matching Linux and the set_termios_mode pattern).
    {
        let guard = TTY_SLOTS[slot].lock();
        match guard.as_ref() {
            Some(tty) if tty.hung_up => return Err(TtyError::HungUp),
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
/// Finishing Phase 8: Full behavioral implementation replacing the Phase 5
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

    use slopos_abi::syscall::{CcIndex, TCIOFF, TCION, TCOOFF, TCOON};

    match action {
        TCOOFF => {
            // Suspend output: set output_stopped flag so writers block.
            let was_stopped = {
                let mut guard = TTY_SLOTS[slot].lock();
                let tty = guard.as_mut().ok_or(TtyError::NotAllocated)?;
                let prev = tty.output_stopped;
                tty.output_stopped = true;
                prev
            };
            // Phase 39: Notify master of flow-control stop transition.
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
                let prev = tty.output_stopped;
                tty.output_stopped = false;
                prev
            };
            // Wake writers and poll waiters regardless — no harm in a
            // spurious wake, and it keeps the logic simple.
            TTY_OUTPUT_WAITERS[slot].wake_all();
            TTY_POLL_WAITERS[slot].wake_all();
            // Phase 39: Notify master of flow-control start transition.
            if was_stopped {
                pty::queue_packet_event(idx, slopos_abi::syscall::TIOCPKT_START);
            }
            Ok(())
        }
        TCIOFF => {
            // Transmit the STOP control byte (VSTOP / XOFF) to the
            // terminal device — the host→device flow-control path.
            let (driver_id, stop_byte) = {
                let guard = TTY_SLOTS[slot].lock();
                let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
                let stop = tty.ldisc.termios().c_cc[CcIndex::Vstop.as_usize()];
                (tty.driver.id(), stop)
            };
            write_driver_unlocked(driver_id, &[stop_byte]);
            Ok(())
        }
        TCION => {
            // Transmit the START control byte (VSTART / XON) to the
            // terminal device — the host→device flow-control path.
            let (driver_id, start_byte) = {
                let guard = TTY_SLOTS[slot].lock();
                let tty = guard.as_ref().ok_or(TtyError::NotAllocated)?;
                let start = tty.ldisc.termios().c_cc[CcIndex::Vstart.as_usize()];
                (tty.driver.id(), start)
            };
            write_driver_unlocked(driver_id, &[start_byte]);
            Ok(())
        }
        _ => Err(TtyError::InvalidArg),
    }
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
        // Finishing Phase 8: Clear output stop so blocked writers unblock
        // and see the hung_up flag on re-check.
        tty.output_stopped = false;
        // Phase 39: full flush → both FLUSHREAD + FLUSHWRITE packet events.
        // Deferred until after lock is dropped to avoid self-deadlock.
        tty.session.detach();
        tty.hung_up = true;
        sid
    };

    // Phase 39: Deliver deferred packet events now that the slot lock is released.
    pty::queue_packet_event(
        idx,
        slopos_abi::syscall::TIOCPKT_FLUSHREAD | slopos_abi::syscall::TIOCPKT_FLUSHWRITE,
    );

    // Phase 15: Signal the entire session (not just fg_pgrp) so that all
    // processes in the session receive SIGHUP + SIGCONT on hangup.
    if session_id != 0 {
        let _ = clear_session_controlling_tty(session_id, idx);
        let _ = signal_session(session_id, SIGHUP);
        let _ = signal_session(session_id, SIGCONT);
    }

    if scheduler_is_enabled() != 0 {
        TTY_INPUT_WAITERS[slot].wake_all();
        // Finishing Phase 1: Wake output waiters and per-slot poll sleepers
        // so they see POLLHUP.
        TTY_OUTPUT_WAITERS[slot].wake_all();
        TTY_POLL_WAITERS[slot].wake_all();
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

/// Phase 41: Revoke access to the caller's controlling terminal.
///
/// This is the kernel-side implementation of the `vhangup()` syscall.
/// It reuses the existing hangup infrastructure (Phase 7/33) to:
/// - Flush buffers and detach the session
/// - Mark the TTY as hung up so subsequent I/O returns EIO
/// - Signal the session with SIGHUP + SIGCONT
/// - Wake all blocked readers/writers
///
/// The caller must provide their controlling terminal index.  Permission
/// checks (caller has a ctty) are enforced by the syscall handler.
pub fn vhangup(idx: TtyIndex) {
    hangup(idx);
}

// ---------------------------------------------------------------------------
// Phase 21: Event-driven poll readiness
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
/// Phase 23: Properly captures and delivers deferred signals from
/// `drain_hw_input_locked()` instead of silently discarding them.
///
/// Only events that are both requested and ready are returned.
pub fn poll_events(idx: TtyIndex, requested: u16) -> u16 {
    use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT};

    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return 0;
    }

    let (deferred_signal, revents) = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(t) => t,
            None => return 0,
        };

        // Drain any pending hardware bytes so has_data() is up-to-date.
        let sig = tty.drain_hw_input_locked();

        let mut revents = 0u16;

        // Phase 39: Packet events also make the master readable.
        if (requested & POLLIN) != 0
            && (tty.ldisc.has_data() || (tty.packet_mode && tty.packet_events != 0))
        {
            revents |= POLLIN;
        }

        if (requested & POLLOUT) != 0 && !tty.ldisc.is_stopped() {
            revents |= POLLOUT;
        }

        // Phase 33: Post-hangup I/O hardening — a hung-up or peer-closed
        // (with no remaining data) TTY reports POLLHUP, POLLIN, and POLLERR.
        // POLLIN is set because a subsequent read() will return immediately
        // with EOF (0 bytes), making the fd "readable".
        // POLLERR is set because a subsequent write() will return -EIO,
        // matching Linux's tty_poll() behaviour.
        if tty.hung_up || (tty.peer_closed && !tty.ldisc.has_data()) {
            revents |= POLLHUP | POLLERR;
            if (requested & POLLIN) != 0 {
                revents |= POLLIN;
            }
        }

        (sig, revents)
    };

    // Phase 23: Deliver deferred signal outside lock to avoid deadlock.
    if let Some((pgid, sig)) = deferred_signal {
        if pgid != 0 {
            let _ = signal_process_group(pgid, sig);
        }
    }

    revents
}

/// Sleep until a poll-relevant event occurs on one of the given TTY slots,
/// or fall back to a short busy-wait if the scheduler is not yet enabled.
///
/// Finishing Phase 1: Per-slot registration.  The caller provides the set
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
    // Collect all active TTY indices.
    let mut slots = [0u8; MAX_TTYS];
    let mut count = 0;
    for i in 0..MAX_TTYS {
        let guard = TTY_SLOTS[i].lock();
        if guard.is_some() {
            slots[count] = i as u8;
            count += 1;
        }
    }
    poll_sleep_on(&slots[..count]);
}

// ---------------------------------------------------------------------------
// Idle callback (Phase 8: iterates ALL active TTYs)
// ---------------------------------------------------------------------------

/// Idle-loop callback: drain hardware input and wake blocked readers.
///
/// Phase 8: now iterates all active TTYs instead of only TTY 0.  Each
/// per-TTY lock is acquired and released individually.
///
/// Phase 23: Properly captures and delivers deferred signals from
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
        // Phase 23: Deliver deferred signal outside lock.
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

fn register_idle_callback() {
    use core::sync::atomic::{AtomicBool, Ordering};
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    register_idle_wakeup_callback(Some(input_available_cb));
}
