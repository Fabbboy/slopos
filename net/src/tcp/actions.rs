//! Side effects the state machine returns to its caller: fixed-size inline
//! arrays, no allocation, and overflow is a programmer error.

use bitflags::bitflags;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::{write_array_field, write_field};

use crate::timer::{TimerKind, TimerToken};

use super::TcpOutSegment;
use super::listener::AcceptedConn;
use super::table::ConnId;

/// Maximum number of outbound segments a single state transition may emit.
pub const MAX_SEGMENTS: usize = 3;

/// Maximum number of timer operations a single state transition may enqueue.
pub const MAX_TIMER_OPS: usize = 4;

/// Everything `Pcb::on_segment` (and the other state-machine entry points)
/// wants the glue layer to do after the table lock is released.
#[derive(Default, Debug, slopos_ostd::SlotFields)]
pub struct Actions {
    /// Outbound segments, in emit order.
    pub segments: [Option<TcpOutSegment>; MAX_SEGMENTS],
    pub segments_len: u8,

    pub timer_ops: [Option<TimerOp>; MAX_TIMER_OPS],
    pub timer_ops_len: u8,

    pub notify: SocketNotify,

    /// The PCB this action set originated from.  State handlers leave it
    /// `None`; the `tcp_input` glue layer fills it in.
    pub conn_id: Option<ConnId>,

    /// Child tuple + seq numbers from a completed 3-way handshake, for the
    /// socket layer to wire into the listener's accept queue.
    pub accepted: Option<AcceptedConn>,

    /// After applying every other action, free this PCB slot.
    pub release: bool,
}

impl Actions {
    pub const fn new() -> Self {
        Self {
            segments: [None, None, None],
            segments_len: 0,
            timer_ops: [None, None, None, None],
            timer_ops_len: 0,
            notify: SocketNotify::empty(),
            conn_id: None,
            accepted: None,
            release: false,
        }
    }

    /// In-place [`Init`] equivalent of [`Self::new`]: the ~400 B rvalue never
    /// materialises on the caller's stack, and the field-by-field writes keep
    /// the closure's own frame inside the 2 KiB stack-size gate.  `AllocError`
    /// is only `KBox::try_init`'s required carrier; the closure never errors.
    pub fn init_default() -> impl Init<Self, AllocError> {
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_array_field!(slot, segments, MAX_SEGMENTS, |_| -> Option<TcpOutSegment> {
                    None
                });
                write_field!(slot, segments_len, 0u8);
                write_array_field!(slot, timer_ops, MAX_TIMER_OPS, |_| -> Option<TimerOp> {
                    None
                });
                write_field!(slot, timer_ops_len, 0u8);
                write_field!(slot, notify, SocketNotify::empty());
                write_field!(slot, conn_id, None);
                write_field!(slot, accepted, None);
                write_field!(slot, release, false);
                Ok(slot.finish())
            },
        )
    }

    /// Prepend `other`'s segments to this set, emptying `other`.  On overflow
    /// the tail is dropped rather than the array growing.
    pub fn merge_segments_from(&mut self, other: &mut Self) {
        if other.segments_len == 0 {
            return;
        }
        let mut merged: [Option<TcpOutSegment>; MAX_SEGMENTS] = [None, None, None];
        let mut len = 0usize;
        for seg in other
            .segments
            .iter_mut()
            .take(other.segments_len as usize)
            .chain(self.segments.iter_mut().take(self.segments_len as usize))
        {
            if len >= MAX_SEGMENTS {
                break;
            }
            merged[len] = seg.take();
            len += 1;
        }
        self.segments = merged;
        self.segments_len = len as u8;
        other.segments_len = 0;
    }

    /// Append an outbound segment.  Panics in debug builds on overflow.
    pub fn push_segment(&mut self, seg: TcpOutSegment) {
        debug_assert!(
            (self.segments_len as usize) < MAX_SEGMENTS,
            "Actions::segments overflow ({} >= {})",
            self.segments_len,
            MAX_SEGMENTS
        );
        if (self.segments_len as usize) < MAX_SEGMENTS {
            self.segments[self.segments_len as usize] = Some(seg);
            self.segments_len += 1;
        }
    }

    /// Append a timer operation.  Panics in debug builds on overflow.
    pub fn push_timer(&mut self, op: TimerOp) {
        debug_assert!(
            (self.timer_ops_len as usize) < MAX_TIMER_OPS,
            "Actions::timer_ops overflow ({} >= {})",
            self.timer_ops_len,
            MAX_TIMER_OPS
        );
        if (self.timer_ops_len as usize) < MAX_TIMER_OPS {
            self.timer_ops[self.timer_ops_len as usize] = Some(op);
            self.timer_ops_len += 1;
        }
    }

    pub fn segments(&self) -> impl Iterator<Item = &TcpOutSegment> {
        self.segments[..self.segments_len as usize]
            .iter()
            .filter_map(|s| s.as_ref())
    }

    pub fn timer_ops(&self) -> impl Iterator<Item = &TimerOp> {
        self.timer_ops[..self.timer_ops_len as usize]
            .iter()
            .filter_map(|op| op.as_ref())
    }
}

bitflags! {
    /// Wake-up / event flags the glue layer feeds into the socket layer
    /// after every `Pcb::on_segment` invocation.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct SocketNotify: u8 {
        /// Wake waiters blocked on `recv()` / `POLLIN`.
        const RECV_WAKE       = 1 << 0;
        /// Wake waiters blocked on `send()` / `POLLOUT`.
        const SEND_WAKE       = 1 << 1;
        /// Wake waiters blocked on `accept()`.
        const ACCEPT_WAKE     = 1 << 2;
        /// The peer sent RST or the stack encountered an unrecoverable
        /// error; the socket surfaces `ECONNRESET` on the next read.
        const RESET_RECEIVED  = 1 << 3;
        /// The peer sent FIN; `recv()` returns 0 once buffered data drains.
        const PEER_CLOSED     = 1 << 4;
        /// Handshake completed: the glue layer allocates a buffer and wires
        /// the child into the listener's accept queue.
        const NEW_ESTABLISHED = 1 << 5;
    }
}

/// A single schedule-or-cancel operation against the net timer wheel.
#[derive(Clone, Copy, Debug)]
pub enum TimerOp {
    Schedule {
        kind: TimerKind,
        key: u32,
        delay_ms: u64,
    },
    Cancel {
        token: TimerToken,
    },
}
