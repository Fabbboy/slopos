//! SlopOS TTY subsystem — per-terminal TTY abstraction, modelled after
//! Linux's `tty_struct` + `n_tty` line discipline.
//!
//! All public functions take an explicit `TtyIndex`; the `TtyServices` function
//! pointers do the `u8 → TtyIndex` conversion at the syscall boundary.
//!
//! Methods with a `*_locked()` suffix run with the slot `SpinLock` already held —
//! acquiring it is the caller's job.

pub mod backing;
pub mod driver;
pub mod ldisc;
pub mod pty;
pub mod session;
pub mod table;
pub mod vconsole;

/// VT100/ANSI escape-sequence parser, re-exported from `slopos-vt` so the kernel
/// virtual console and the userland terminal emulator share one state machine.
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

/// Re-exported from the ABI crate so it is the kernel's single definition.
pub use slopos_abi::syscall::TtyIndex;

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

pub struct Tty {
    pub(crate) index: TtyIndex,

    pub(crate) ldisc: LdiscKind,

    pub(crate) driver: TtyDriverKind,

    pub(crate) session: TtySession,

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

    #[inline]
    pub(crate) fn add_signal(&mut self, pgrp: KArc<ProcessGroup>, signum: u8) {
        self.signal = Some((pgrp, signum));
    }

    /// Ask for `slot`'s staged echo to be emitted once its guard drops. Only ever
    /// the slot whose own guard the caller holds: flushing a peer would take its
    /// write lock while holding this one's — the inverse of the legal nesting.
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

    /// Throw accumulated work away without running it, for a test that only
    /// inspects it; silently dropping staged work is what `Drop` asserts against.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn discard(mut self) {
        #[cfg(debug_assertions)]
        {
            self.executed = true;
        }
        let _ = &mut self;
    }

    pub(crate) fn execute(mut self) {
        // `mut` is used only by the debug-only cfg block below.
        let _ = &mut self;
        #[cfg(debug_assertions)]
        {
            self.executed = true;
        }
        use slopos_kernel_services::driver_runtime::signal_process_group;

        // Echo first: `^C` must reach the terminal before the SIGINT it announces.
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
            // Poll waiters register on both queues, so wake both.
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

pub use slopos_abi::tty_error::TtyError;

pub use self::io::{
    bytes_available, has_data, output_queued_bytes, push_input, push_input_batch, read,
    read_with_attach, write,
};

pub use self::io::{
    get_packet_mode, get_pty_lock, get_pty_number, is_pty_slave, is_slave_locked, pty_alloc,
    queue_packet_event, set_packet_mode, set_pty_lock,
};

pub use self::termios::{
    get_ldisc, get_termios, get_winsize, is_output_idle, set_ldisc, set_termios, set_termios_flush,
    set_termios_wait, set_winsize, tcflush, tcsbrk, tcxonc,
};

pub use self::job_control::{
    acquire_controlling_terminal, attach_session, detach_controlling_terminal, detach_session,
    get_foreground_pgrp, get_session_id, release_controlling_terminal, set_foreground_pgrp,
    set_foreground_pgrp_checked,
};

pub use self::backing::{TtyBacking, TtySlaveOpen, open_tty, pty_open_peer, pty_open_slave};

#[cfg(feature = "test-hooks")]
pub use self::lifecycle::clear_hangup;
pub use self::lifecycle::{
    active_tty, default_console_tty, get_exclusive, hangup, init, is_hung_up, set_active_tty,
    set_default_console_tty, set_exclusive, switch_active_tty, vhangup,
};

pub use self::poll::{
    get_compositor_focus, poll_dequeue, poll_enqueue, poll_events, set_compositor_focus,
};

pub use self::session::detach_session_by_id;
