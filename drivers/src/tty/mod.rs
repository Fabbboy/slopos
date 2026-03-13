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
//! locking.
//!
//! # Public API
//!
//! All public functions take an explicit `TtyIndex` — there are no global
//! shims.  The `TtyServices` function pointers (registered in
//! `syscall_services_init.rs`) perform the `u8 → TtyIndex` conversion at the
//! boundary.
//!
//! # Locking Convention
//!
//! Methods that operate on a `Tty` while the slot `IrqMutex` is already held
//! use the `*_locked()` suffix (e.g. `drain_hw_input_locked`).  This makes the
//! caller responsible for acquiring the lock and documents the precondition at
//! the call site.
//!
//! # Module Organisation
//!
//! The implementation is decomposed into focused sub-modules:
//!
//! - [`io`] — read, write, push_input, hardware drain, data queries
//! - [`termios`] — termios get/set, window size, ldisc, ioctls, drain
//! - [`job_control`] — session, foreground pgrp, controlling terminal
//! - [`lifecycle`] — open/close ref counting, hangup, active TTY, init
//! - [`poll`] — poll readiness, poll sleep, compositor focus

// Existing sub-modules (unchanged)
pub mod driver;
pub mod ldisc;
pub mod pty;
pub mod ringbuf;
pub mod session;
pub mod table;
pub mod vconsole;
pub mod vtparser;

// Decomposed sub-modules
mod io;
mod job_control;
mod lifecycle;
mod poll;
mod termios;

use bitflags::bitflags;
use slopos_abi::syscall::UserWinsize;

use self::driver::{DriverId, TtyDriverKind, write_driver_unlocked};
use self::ldisc::LdiscKind;
use self::session::TtySession;
use self::table::{TTY_INPUT_WAITERS, TTY_OUTPUT_WAITERS, TTY_POLL_WAITERS};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Re-export `TtyIndex` from the ABI crate so that it is the single
/// definition used across the entire kernel.
pub use slopos_abi::syscall::TtyIndex;

/// Maximum number of TTY instances.
pub const MAX_TTYS: usize = 32;

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct TtyFlags: u16 {
        const HUNG_UP = 1 << 0;
        const PEER_CLOSED = 1 << 1;
        const SLAVE_LOCKED = 1 << 2;
        const PACKET_MODE = 1 << 3;
        const THROTTLED = 1 << 4;
        const OUTPUT_STOPPED = 1 << 5;
        const EXCLUSIVE = 1 << 6;
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct PacketEvents: u8 {
        const FLUSHREAD = slopos_abi::syscall::TIOCPKT_FLUSHREAD as u8;
        const FLUSHWRITE = slopos_abi::syscall::TIOCPKT_FLUSHWRITE as u8;
        const STOP = slopos_abi::syscall::TIOCPKT_STOP as u8;
        const START = slopos_abi::syscall::TIOCPKT_START as u8;
        const NOSTOP = slopos_abi::syscall::TIOCPKT_NOSTOP as u8;
        const DOSTOP = slopos_abi::syscall::TIOCPKT_DOSTOP as u8;
    }
}

/// The central TTY structure — one per terminal.
pub struct Tty {
    /// Which TTY slot this is (0 = serial console, 1 = virtual console, etc.).
    /// Read in tests; suppressed dead_code since it's pub(crate).
    #[allow(dead_code)]
    pub(crate) index: TtyIndex,

    /// The line discipline owned by this TTY.
    pub(crate) ldisc: LdiscKind,

    /// Hardware driver backend.
    pub(crate) driver: TtyDriverKind,

    /// Session/foreground state (includes focused_task_id).
    pub(crate) session: TtySession,

    /// Window size (for TIOCGWINSZ / TIOCSWINSZ).
    pub(crate) winsize: UserWinsize,
    pub(crate) open_count: u32,
    pub(crate) flags: TtyFlags,
    pub(crate) packet_events: PacketEvents,
}

impl Tty {
    pub(crate) fn mark_hung_up(&mut self) {
        self.flags.insert(TtyFlags::HUNG_UP);
        self.flags.remove(TtyFlags::OUTPUT_STOPPED);
        debug_assert!(!self.flags.contains(TtyFlags::OUTPUT_STOPPED));
    }
}

// ---------------------------------------------------------------------------
// PostLockWork — RAII helper for deferred actions after lock release
// ---------------------------------------------------------------------------

/// Accumulates work that must be performed **after** dropping the per-TTY
/// lock, to avoid deadlock or lock-ordering violations.
///
/// The repeated pattern of "capture signal/IXOFF byte/packet event inside
/// lock → deliver after lock drop" appears ~8 times in `io.rs` alone and
/// in `poll.rs`, `lifecycle.rs`, and `termios.rs`.  `PostLockWork` replaces
/// all manual deferred-delivery boilerplate with a single RAII struct:
///
/// ```ignore
/// let mut deferred = PostLockWork::new();
/// {
///     let mut guard = TTY_SLOTS[slot].lock();
///     // ... work under lock, accumulate into `deferred` ...
///     deferred.add_signal(pgid, signum);
/// }
/// deferred.execute();  // delivers everything outside the lock
/// ```
#[derive(Default)]
pub(crate) struct PostLockWork {
    /// Deferred signal delivery: `(pgid, signum)`.
    signal: Option<(u32, u8)>,
    /// Deferred IXOFF/IXON byte to send to a driver.
    ixoff_byte: Option<(DriverId, u8)>,
    /// Deferred packet event to queue on a slave's master.
    packet_event: Option<(TtyIndex, u8)>,
    /// Slot indices where `TTY_INPUT_WAITERS` should be woken.
    wake_input: [bool; MAX_TTYS],
    /// Slot indices where `TTY_OUTPUT_WAITERS` should be woken.
    wake_output: [bool; MAX_TTYS],
    /// Slot indices where `TTY_POLL_WAITERS` should be woken.
    wake_poll: [bool; MAX_TTYS],
}

impl PostLockWork {
    /// Create a new empty deferred work accumulator.
    pub(crate) const fn new() -> Self {
        Self {
            signal: None,
            ixoff_byte: None,
            packet_event: None,
            wake_input: [false; MAX_TTYS],
            wake_output: [false; MAX_TTYS],
            wake_poll: [false; MAX_TTYS],
        }
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.signal.is_none()
            && self.ixoff_byte.is_none()
            && self.packet_event.is_none()
            && !self.wake_input.iter().any(|&x| x)
            && !self.wake_output.iter().any(|&x| x)
            && !self.wake_poll.iter().any(|&x| x)
    }

    /// Queue a signal for delivery to a process group.
    #[inline]
    pub(crate) fn add_signal(&mut self, pgid: u32, signum: u8) {
        if pgid != 0 {
            self.signal = Some((pgid, signum));
        }
    }

    /// Queue an IXOFF/IXON byte to send to a driver.
    #[inline]
    pub(crate) fn add_ixoff_byte(&mut self, driver_id: DriverId, byte: u8) {
        self.ixoff_byte = Some((driver_id, byte));
    }

    /// Queue a packet event to deliver to a slave's paired master.
    #[inline]
    pub(crate) fn add_packet_event(&mut self, slave_idx: TtyIndex, event_bits: u8) {
        if event_bits != 0 {
            match self.packet_event {
                Some((idx, ref mut bits)) if idx == slave_idx => {
                    *bits |= event_bits;
                }
                _ => {
                    self.packet_event = Some((slave_idx, event_bits));
                }
            }
        }
    }

    /// Mark a slot for input waiter wakeup.
    #[inline]
    pub(crate) fn wake_input_slot(&mut self, slot: usize) {
        if slot < MAX_TTYS {
            self.wake_input[slot] = true;
        }
    }

    /// Mark a slot for output waiter wakeup.
    #[inline]
    pub(crate) fn wake_output_slot(&mut self, slot: usize) {
        if slot < MAX_TTYS {
            self.wake_output[slot] = true;
        }
    }

    /// Mark a slot for poll waiter wakeup.
    #[inline]
    pub(crate) fn wake_poll_slot(&mut self, slot: usize) {
        if slot < MAX_TTYS {
            self.wake_poll[slot] = true;
        }
    }

    /// Convenience: wake both output and poll waiters on a slot.
    #[inline]
    pub(crate) fn wake_output_and_poll(&mut self, slot: usize) {
        self.wake_output_slot(slot);
        self.wake_poll_slot(slot);
    }

    /// Convenience: wake input and poll waiters on a slot.
    #[inline]
    pub(crate) fn wake_input_and_poll(&mut self, slot: usize) {
        self.wake_input_slot(slot);
        self.wake_poll_slot(slot);
    }

    /// Execute all accumulated deferred work.
    ///
    /// **MUST be called after dropping all per-TTY locks.**
    pub(crate) fn execute(self) {
        use slopos_lib::kernel_services::driver_runtime::signal_process_group;

        // 1. Deliver deferred signal.
        if let Some((pgid, sig)) = self.signal {
            let _ = signal_process_group(pgid, sig);
        }

        // 2. Send IXOFF/IXON byte to driver.
        if let Some((driver_id, byte)) = self.ixoff_byte {
            write_driver_unlocked(driver_id, &[byte]);
        }

        // 3. Queue packet event on master.
        if let Some((slave_idx, event_bits)) = self.packet_event {
            pty::queue_packet_event(slave_idx, event_bits);
        }

        // 4. Wake waiters.
        for slot in 0..MAX_TTYS {
            if self.wake_input[slot] {
                TTY_INPUT_WAITERS[slot].wake_all();
            }
            if self.wake_output[slot] {
                TTY_OUTPUT_WAITERS[slot].wake_all();
            }
            if self.wake_poll[slot] {
                TTY_POLL_WAITERS[slot].wake_all();
            }
        }
    }
}

/// Kernel-internal error type for TTY operations.
///
/// # `to_errno()` boundary mapping
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
    /// session — hard denial.
    CrossSessionDenied,
    /// Operation was interrupted by a signal.
    SignalInterrupt,
    /// Background process in an orphaned process group tried to change
    /// terminal settings — returns EIO instead of SIGTTOU.
    OrphanedProcessGroup,
    /// Invalid argument.
    InvalidArg,
    /// Device is in exclusive mode and already open.
    DeviceBusy,
    /// Blocking syscall was interrupted by a signal and
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
            TtyError::DeviceBusy => ERRNO_EBUSY as i32,
            TtyError::Restart => ERRNO_ERESTARTSYS as i32,
        }
    }
}

// ---------------------------------------------------------------------------
// Re-exports from decomposed sub-modules (preserves public API)
// ---------------------------------------------------------------------------

// io.rs: I/O paths
pub use self::io::{
    bytes_available, has_data, output_queued_bytes, push_input, push_input_batch, read,
    read_with_attach, write,
};

// io.rs: PTY re-exports (originally in mod.rs, routed through io.rs)
pub use self::io::{
    get_packet_mode, get_pty_lock, get_pty_number, is_pty_slave, is_slave_locked, pty_alloc,
    pty_open_slave, queue_packet_event, set_packet_mode, set_pty_lock,
};

// termios.rs: terminal configuration and control ioctls
pub use self::termios::{
    get_ldisc, get_termios, get_winsize, is_output_idle, set_ldisc, set_termios, set_termios_flush,
    set_termios_wait, set_winsize, tcflush, tcsbrk, tcxonc,
};

// job_control.rs: session and foreground pgrp management
pub use self::job_control::{
    acquire_controlling_terminal, attach_session, detach_controlling_terminal, detach_session,
    get_foreground_pgrp, get_session_id, release_controlling_terminal, set_foreground_pgrp,
    set_foreground_pgrp_checked,
};

// lifecycle.rs: open/close, hangup, active TTY, init, exclusive mode
pub use self::lifecycle::{
    active_tty, close_ref, default_console_tty, get_exclusive, hangup, init, is_hung_up, open_ref,
    set_active_tty, set_default_console_tty, set_exclusive, switch_active_tty, vhangup,
};

// poll.rs: poll readiness and compositor focus
pub use self::poll::{
    get_compositor_focus, poll_events, poll_sleep, poll_sleep_on, set_compositor_focus,
};

// session.rs: direct re-export
pub use self::session::detach_session_by_id;
