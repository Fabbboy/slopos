//! SlopOS TTY subsystem — per-terminal TTY abstraction, modelled after
//! Linux's `tty_struct` + `n_tty` line discipline.
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
//! instances, each with its own `SpinLock` for fully independent per-TTY
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
//! Methods that operate on a `Tty` while the slot `SpinLock` is already held
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
//! - [`lifecycle`] — hangup, vhangup, active TTY routing, init
//! - [`poll`] — poll readiness, poll sleep, compositor focus

pub mod backing;
pub mod driver;
pub mod ldisc;
pub mod pty;
pub mod session;
pub mod table;
pub mod vconsole;

/// VT100/ANSI escape sequence parser, re-exported from the standalone
/// `slopos-vt` crate so the kernel virtual console and the userland terminal
/// emulator share one state machine.
pub mod vtparser {
    pub use slopos_vt::{Direction, EraseMode, SgrAttr, VtAction, VtParser};
}

pub(crate) mod io;
mod job_control;
mod lifecycle;
pub(crate) mod output;
mod poll;
mod termios;

use bitflags::bitflags;
use slopos_abi::syscall::UserWinsize;

use self::driver::TtyDriverKind;
use self::ldisc::LdiscKind;
use self::output::WriteNesting;
use self::session::TtySession;
use self::table::{tty_input_event, tty_output_event};
use slopos_ostd::KArc;
use slopos_ostd::sync::BUS;
use slopos_ostd::task::ProcessGroup;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Re-export `TtyIndex` from the ABI crate so that it is the single
/// definition used across the entire kernel.
pub use slopos_abi::syscall::TtyIndex;

/// Maximum number of TTY instances.
pub use slopos_abi::event::MAX_TTYS;

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
    pub(crate) index: TtyIndex,

    /// The line discipline owned by this TTY.
    pub(crate) ldisc: LdiscKind,

    /// Hardware driver backend.
    pub(crate) driver: TtyDriverKind,

    /// Session/foreground state (includes focused_task_id).
    pub(crate) session: TtySession,

    /// Window size (for TIOCGWINSZ / TIOCSWINSZ).
    pub(crate) winsize: UserWinsize,
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

impl Drop for Tty {
    fn drop(&mut self) {
        self.ldisc.flush_all();
        self.session.detach();
    }
}

// ---------------------------------------------------------------------------
// PostLockWork — RAII helper for deferred actions after lock release
// ---------------------------------------------------------------------------

/// Accumulates work that must run **after** the per-TTY lock drops, because it
/// emits output, delivers a signal, or wakes a waiter.
///
/// ```ignore
/// let mut deferred = PostLockWork::new();
/// {
///     let mut guard = TTY_SLOTS[slot].lock();
///     // ... work under lock, accumulate into `deferred` ...
/// }
/// deferred.execute();
/// ```
pub(crate) struct PostLockWork {
    /// Foreground group to signal, pinned strongly so the group's identity
    /// survives across the off-lock delivery scan (no reused-pid window).
    signal: Option<(KArc<ProcessGroup>, u8)>,
    /// Slots whose line discipline has echo staged for emission.
    echo_flush: u32,
    /// Nesting the staged emissions run under.  One value covers the mask:
    /// a peer-nested batch reaches exactly one slot, the peer's.
    echo_nesting: WriteNesting,
    packet_event: Option<(TtyIndex, u8)>,
    wake_input: u32,
    wake_output: u32,
    wake_poll: u32,
    #[cfg(debug_assertions)]
    executed: bool,
}

const _: () = assert!(
    MAX_TTYS <= u32::BITS as usize,
    "PostLockWork tracks slots in a u32 bitmask"
);

impl PostLockWork {
    /// Create a new empty deferred work accumulator.
    pub(crate) const fn new() -> Self {
        Self {
            signal: None,
            echo_flush: 0,
            echo_nesting: WriteNesting::Toplevel,
            packet_event: None,
            wake_input: 0,
            wake_output: 0,
            wake_poll: 0,
            #[cfg(debug_assertions)]
            executed: false,
        }
    }

    #[cfg_attr(
        not(any(feature = "test-hooks", debug_assertions)),
        expect(dead_code, reason = "used in test-hooks and debug Drop impl")
    )]
    pub(crate) fn is_empty(&self) -> bool {
        self.signal.is_none()
            && self.echo_flush == 0
            && self.packet_event.is_none()
            && self.wake_input == 0
            && self.wake_output == 0
            && self.wake_poll == 0
    }

    /// Queue a signal for delivery to a (live) foreground process group.
    #[inline]
    pub(crate) fn add_signal(&mut self, pgrp: KArc<ProcessGroup>, signum: u8) {
        self.signal = Some((pgrp, signum));
    }

    /// Ask for `slot`'s staged echo to be emitted once its guard drops.
    ///
    /// Only ever the slot whose own guard the caller holds: flushing a peer
    /// would take its write lock while holding this one's, the inverse of the
    /// single legal nesting direction.
    #[inline]
    pub(crate) fn request_echo_flush(&mut self, slot: usize, nesting: WriteNesting) {
        if slot < MAX_TTYS {
            self.echo_flush |= 1 << slot;
            if nesting == WriteNesting::PeerNested {
                self.echo_nesting = nesting;
            }
        }
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

    #[inline]
    pub(crate) fn wake_input_slot(&mut self, slot: usize) {
        if slot < MAX_TTYS {
            self.wake_input |= 1 << slot;
        }
    }

    #[inline]
    pub(crate) fn wake_output_slot(&mut self, slot: usize) {
        if slot < MAX_TTYS {
            self.wake_output |= 1 << slot;
        }
    }

    #[inline]
    pub(crate) fn wake_poll_slot(&mut self, slot: usize) {
        if slot < MAX_TTYS {
            self.wake_poll |= 1 << slot;
        }
    }

    #[inline]
    pub(crate) fn wake_output_and_poll(&mut self, slot: usize) {
        self.wake_output_slot(slot);
        self.wake_poll_slot(slot);
    }

    #[inline]
    pub(crate) fn wake_input_and_poll(&mut self, slot: usize) {
        self.wake_input_slot(slot);
        self.wake_poll_slot(slot);
    }

    /// Throw the accumulated work away without running it, for a test that
    /// built an accumulator only to inspect it.  Dropping staged work silently
    /// is the failure mode the `Drop` assertion exists to catch, so production
    /// code has no reason to reach for this.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn discard(mut self) {
        #[cfg(debug_assertions)]
        {
            self.executed = true;
        }
        let _ = &mut self;
    }

    pub(crate) fn execute(mut self) {
        // `mut` is needed in debug builds for the assertion tracking below.
        // In release the cfg block is stripped, making `mut` appear unused.
        let _ = &mut self;
        #[cfg(debug_assertions)]
        {
            self.executed = true;
        }
        use slopos_kernel_services::driver_runtime::signal_process_group;

        // Echo first: a signal character echoes its caret form, and `^C` has
        // to reach the terminal before the SIGINT it announces.
        let mut bits = self.echo_flush;
        while bits != 0 {
            let slot = bits.trailing_zeros() as usize;
            output::flush_echo(slot, self.echo_nesting);
            bits &= bits - 1;
        }

        if let Some((pgrp, sig)) = self.signal.take() {
            let _ = signal_process_group(pgrp.id(), sig);
        }

        if let Some((slave_idx, event_bits)) = self.packet_event {
            pty::queue_packet_event(slave_idx, event_bits);
        }

        let mut bits = self.wake_input;
        while bits != 0 {
            let slot = bits.trailing_zeros() as usize;
            BUS.publish(tty_input_event(slot));
            bits &= bits - 1;
        }
        bits = self.wake_output;
        while bits != 0 {
            let slot = bits.trailing_zeros() as usize;
            BUS.publish(tty_output_event(slot));
            bits &= bits - 1;
        }
        bits = self.wake_poll;
        while bits != 0 {
            let slot = bits.trailing_zeros() as usize;
            // Poll waiters register on both the input and output queues, so
            // wake both to cover either readiness direction.
            BUS.publish(tty_input_event(slot));
            BUS.publish(tty_output_event(slot));
            bits &= bits - 1;
        }
    }
}

#[cfg(debug_assertions)]
impl Drop for PostLockWork {
    fn drop(&mut self) {
        debug_assert!(
            self.executed || self.is_empty(),
            "PostLockWork dropped with pending deferred work — call .execute()"
        );
    }
}

/// Kernel-internal error type for TTY operations.
pub use slopos_abi::tty_error::TtyError;

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
    queue_packet_event, set_packet_mode, set_pty_lock,
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

// backing.rs: open/close lifetime — clone/drop of the owning references
pub use self::backing::{TtyBacking, TtySlaveOpen, open_tty, pty_open_peer, pty_open_slave};

// lifecycle.rs: hangup, active TTY, init, exclusive mode
pub use self::lifecycle::{
    active_tty, default_console_tty, get_exclusive, hangup, init, is_hung_up, set_active_tty,
    set_default_console_tty, set_exclusive, switch_active_tty, vhangup,
};

// poll.rs: poll readiness and compositor focus
pub use self::poll::{
    get_compositor_focus, poll_dequeue, poll_enqueue, poll_events, poll_sleep, poll_sleep_on,
    set_compositor_focus,
};

// session.rs: direct re-export
pub use self::session::detach_session_by_id;
