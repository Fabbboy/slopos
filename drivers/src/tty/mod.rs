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

use slopos_abi::syscall::UserWinsize;

use self::driver::TtyDriverKind;
use self::ldisc::LdiscKind;
use self::session::TtySession;

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

    /// PTY slave lock state.  When `true`, the corresponding
    /// `/dev/pts/N` device node cannot be opened.  Only meaningful for
    /// PTY slaves (always `false` for consoles and masters).  Defaults to
    /// `true` on `pty_alloc()` — the master holder must unlock via
    /// `TIOCSPTLCK` before the slave can be opened.
    pub slave_locked: bool,

    /// PTY packet mode.  When `true` on a PTY master, every
    /// `read()` is prefixed with a single control byte indicating the
    /// event type (see `TIOCPKT_*` constants in `abi`).
    pub packet_mode: bool,

    /// Pending packet-mode event bits.  Bitwise OR of
    /// `TIOCPKT_FLUSHREAD`, `TIOCPKT_FLUSHWRITE`, `TIOCPKT_STOP`,
    /// `TIOCPKT_START`, `TIOCPKT_NOSTOP`, `TIOCPKT_DOSTOP`.
    /// Consumed on the next master `read()` when non-zero.
    pub packet_events: u8,

    /// PTY flow control throttle flag.  When `true`,
    /// the slave's cooked buffer has exceeded `THROTTLE_HIGH_WATER` and
    /// the master-side writer must be back-pressured (blocked or EAGAIN).
    /// Cleared when a slave `read()` drains below `THROTTLE_LOW_WATER`.
    pub throttled: bool,

    /// Explicit output-stop state for TCXONC.  When `true`,
    /// `tty_write()` blocks (or returns EAGAIN for non-blocking) until output
    /// is resumed via `tcxonc(TCOON)`.  Separate from the ldisc `stopped`
    /// flag which is driven by IXON (Ctrl+S / Ctrl+Q keyboard flow control).
    pub output_stopped: bool,
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

// lifecycle.rs: open/close, hangup, active TTY, init
pub use self::lifecycle::{
    active_tty, close_ref, default_console_tty, hangup, init, is_hung_up, open_ref, set_active_tty,
    set_default_console_tty, switch_active_tty, vhangup,
};

// poll.rs: poll readiness and compositor focus
pub use self::poll::{
    get_compositor_focus, poll_events, poll_sleep, poll_sleep_on, set_compositor_focus,
};

// session.rs: direct re-export
pub use self::session::detach_session_by_id;
