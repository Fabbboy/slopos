//! Side effects the state machine returns to its caller.
//!
//! Every [`crate::tcp::pcb::Pcb::on_segment`] invocation is a pure
//! `(&mut self, Segment, now_ms) -> Actions` function.  It may mutate the
//! PCB in place, but every external effect — outgoing segments to send,
//! timers to (re)schedule or cancel, socket-layer waiters to wake, and
//! PCB slot releases — lands in an `Actions` struct that the glue layer
//! applies after the table lock is dropped.
//!
//! ## Size and shape
//!
//! `Actions` is a plain struct of fixed-size inline arrays — no `Vec`,
//! no `Box`, no allocation.  The worst-case output from a single input
//! segment is ~3 segments (seen in SYN+ACK retransmit or a
//! simultaneous-close crossover), so the `segments` slot is `[_; 3]` and
//! `timer_ops` is `[_; 4]` (cancel old + schedule new for each of
//! retransmit / delayed ACK / keepalive / TIME_WAIT).  Overflow is a
//! programmer error caught by `debug_assert!` inside [`Actions::push_segment`]
//! / [`Actions::push_timer`].

use bitflags::bitflags;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, SlotPtr, init_struct_with};
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
    /// Outbound segments, in emit order.  Use [`Actions::push_segment`] to
    /// append; use [`Actions::drain_segments`] to iterate in the glue layer.
    pub segments: [Option<TcpOutSegment>; MAX_SEGMENTS],
    pub segments_len: u8,

    /// Timer operations — schedule or cancel entries in the net timer wheel.
    pub timer_ops: [Option<TimerOp>; MAX_TIMER_OPS],
    pub timer_ops_len: u8,

    /// Bitfield of socket-layer wake-ups / side effects.
    pub notify: SocketNotify,

    /// The PCB this action set originated from.  State handlers leave
    /// this as `None`; the `tcp_input` glue layer fills it in so
    /// consumers (socket layer, drivers) know which connection to act on.
    pub conn_id: Option<ConnId>,

    /// When a LISTEN PCB accepts a new child via a completed 3-way
    /// handshake, the child's tuple + seq numbers land here so the socket
    /// layer can wire it into the accept queue.  Replaces the old
    /// `TcpInputResult::accepted_idx` pattern with richer metadata.
    pub accepted: Option<AcceptedConn>,

    /// After applying every other action, free this PCB slot.  Replaces
    /// the ad-hoc `table.release(idx)` calls scattered through the old
    /// `process_*` handlers.
    pub release: bool,
}

impl Actions {
    /// Create an empty action set.  Equivalent to `Actions::default()` but
    /// can be called from `const` context.
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

    /// In-place [`Init`] recipe equivalent to [`Self::new`]. Used by
    /// `KBox::try_init(Actions::init_default())` so a caller that
    /// wants a heap-resident, reusable `Actions` slot does not have
    /// to materialise the ~400 B `Self::new()` rvalue on its own
    /// stack frame first. The hand-written field-by-field init keeps
    /// the closure's frame inside the 2 KiB stack-size gate.
    ///
    /// `AllocError` is the carrier required by `KBox::try_init`'s
    /// `E: From<AllocError>` bound — the closure itself never errors.
    pub fn init_default() -> impl Init<Self, AllocError> {
        // Writes every field of `Self` exactly once via the safe
        // field-writer wrappers. The `[Option<_>; N]` arrays are
        // initialised element-by-element so no by-value array literal
        // materialises in this closure.
        init_struct_with(|slot: SlotPtr<Self>| -> Result<(), AllocError> {
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
            Ok(())
        })
    }

    /// Append an outbound segment.  Panics in debug builds if the inline
    /// capacity is exceeded — a state transition emitting more than
    /// [`MAX_SEGMENTS`] is a bug.
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

    /// Iterate over the valid outbound segments in emit order.
    pub fn segments(&self) -> impl Iterator<Item = &TcpOutSegment> {
        self.segments[..self.segments_len as usize]
            .iter()
            .filter_map(|s| s.as_ref())
    }

    /// Iterate over the valid timer operations in order.
    pub fn timer_ops(&self) -> impl Iterator<Item = &TimerOp> {
        self.timer_ops[..self.timer_ops_len as usize]
            .iter()
            .filter_map(|op| op.as_ref())
    }
}

bitflags! {
    /// Wake-up / event flags the glue layer feeds into the socket layer
    /// after every `Pcb::on_segment` invocation.  Multiple flags may be
    /// set on a single transition (for example, a handshake completion
    /// sets both `RECV_WAKE` and `NEW_ESTABLISHED`).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct SocketNotify: u8 {
        /// Wake waiters blocked on `recv()` / `POLLIN`.  Set whenever new
        /// bytes land in the recv buffer or the connection enters a state
        /// where further reads are impossible (EOF / reset).
        const RECV_WAKE       = 1 << 0;
        /// Wake waiters blocked on `send()` / `POLLOUT`.
        const SEND_WAKE       = 1 << 1;
        /// Wake waiters blocked on `accept()`.
        const ACCEPT_WAKE     = 1 << 2;
        /// The peer sent RST or the stack encountered an unrecoverable
        /// error; the socket should surface `ECONNRESET` on the next read.
        const RESET_RECEIVED  = 1 << 3;
        /// The peer closed its half of the connection (FIN).  The socket
        /// can return 0 from `recv()` once buffered data is drained.
        const PEER_CLOSED     = 1 << 4;
        /// The connection just completed its 3-way handshake.  Signals
        /// the glue layer to allocate a buffer and wire the child into
        /// the listener's accept queue.
        const NEW_ESTABLISHED = 1 << 5;
    }
}

/// A single schedule-or-cancel operation against the net timer wheel.
///
/// Wrapped in an `Option<TimerOp>` inside [`Actions::timer_ops`] so the
/// slot can be empty without allocating a default `Cancel(TimerToken::NONE)`.
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
