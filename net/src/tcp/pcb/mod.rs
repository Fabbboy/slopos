//! Process control block — the state machine each TCP connection lives
//! inside.
//!
//! Every active connection owns one [`Pcb`]: a common header plus a
//! variant-sized [`PcbState`] payload, with each per-state handler in its own
//! submodule and this module holding the `Pcb::on_segment` dispatcher.
//!
//! Transitions mutate `self.state` in place rather than consuming `self`, so
//! `PcbTable` can keep a `Pcb` where it sits in its
//! `[Option<Pcb>; MAX_CONNECTIONS]` slot with no `take()`/put-back dance.

pub mod data;
pub mod listen;
pub mod syn_recv;
pub mod syn_sent;
pub mod time_wait;

pub use data::{ClosePhase, DataState};
pub use listen::ListenState;
pub use syn_recv::SynRecvState;
pub use syn_sent::SynSentState;
pub use time_wait::TimeWaitState;

use crate::tcp::actions::Actions;
use crate::tcp::buffer::TcpBufferPair;
use crate::tcp::header::TcpHeader;
use crate::tcp::tuple::TcpTuple;

/// Opaque handle the PCB uses to refer back to its owning socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SocketId(pub u32);

/// Process control block.  One of these per active TCP connection.
///
/// Stored inside `[Option<Pcb>; MAX_CONNECTIONS]` behind
/// `SpinLock<PcbTable>` (see [`crate::tcp::table`]); the glue layer in
/// `tcp::input` drains the returned [`Actions`] after that lock is released.
#[derive(Debug)]
pub struct Pcb {
    pub tuple: TcpTuple,

    /// Owning socket; `None` for anonymous server-side children before
    /// `accept()`.
    pub socket_id: Option<SocketId>,

    pub state: PcbState,
}

impl Pcb {
    pub const fn new(tuple: TcpTuple, state: PcbState) -> Self {
        Self {
            tuple,
            socket_id: None,
            state,
        }
    }

    /// Apply a decoded incoming segment to this PCB, returning the [`Actions`]
    /// the glue layer should apply after the table lock is dropped.
    ///
    /// `incoming` is the segment's real four-tuple. For every state but
    /// `Listen` it equals `self.tuple`; a listener's tuple carries a wildcard
    /// remote, so the peer's address has to come from the IPv4 layer.
    pub fn on_segment(
        &mut self,
        bufs: Option<&mut TcpBufferPair>,
        incoming: &TcpTuple,
        hdr: &TcpHeader,
        options: &[u8],
        payload: &[u8],
        now_ms: u64,
    ) -> Actions {
        self.assert_invariants();
        let actions = match &mut self.state {
            PcbState::Listen(_) => {
                listen::ListenState::on_segment(self, incoming, hdr, options, now_ms)
            }
            PcbState::SynSent(_) => syn_sent::SynSentState::on_segment(self, hdr, options, now_ms),
            PcbState::SynRecv(_) => syn_recv::SynRecvState::on_segment(self, hdr, now_ms),
            PcbState::Data(_) => {
                let bufs = bufs.expect("Data state must have an allocated buffer");
                data::DataState::on_segment(self, bufs, hdr, options, payload, now_ms)
            }
            PcbState::TimeWait(_) => time_wait::TimeWaitState::on_segment(self, hdr, now_ms),
        };
        self.assert_invariants();
        actions
    }

    /// Debug-only invariant audit; buffer-lifecycle invariants are checked at
    /// the table level, not here.
    #[inline]
    pub fn assert_invariants(&self) {
        #[cfg(debug_assertions)]
        match &self.state {
            PcbState::Listen(_) | PcbState::TimeWait(_) => {}
            PcbState::SynSent(s) => s.debug_assert_invariants(self),
            PcbState::SynRecv(s) => s.debug_assert_invariants(self),
            PcbState::Data(s) => s.debug_assert_invariants(self),
        }
    }
}

/// Per-state payload, roughly the RFC 793 state diagram with the four closing
/// substates (`FIN_WAIT_1`, `FIN_WAIT_2`, `CLOSING`, `LAST_ACK`) folded into
/// [`DataState`] via [`data::ClosePhase`]: they share every `DataState` field
/// and differ only in which FIN flags have been exchanged.
#[derive(Debug)]
pub enum PcbState {
    /// Passive open; children in `SYN_RECEIVED` are tracked by
    /// `tcp::listener::TcpListenState`'s two-queue model, not this enum.
    Listen(ListenState),
    /// Active open in progress — we sent a SYN and are waiting for
    /// SYN+ACK.
    SynSent(SynSentState),
    /// Server-side passive open that has received a SYN and sent
    /// SYN+ACK; waiting for the final ACK to complete the handshake.
    SynRecv(SynRecvState),
    /// Data transfer + graceful close.  Covers the RFC 793 states
    /// `ESTABLISHED`, `FIN_WAIT_1`, `FIN_WAIT_2`, `CLOSE_WAIT`,
    /// `CLOSING`, and `LAST_ACK` — see [`data::ClosePhase`].
    ///
    /// Boxed because `DataState` is ~3 KiB while the other variants are
    /// ≤100 bytes, which would otherwise size every static table slot.
    /// Constructed via `KBox::try_init` from [`DataState::init_new`] /
    /// [`DataState::init_from_syn_recv`] so the rvalue never lands on a
    /// caller's stack.
    Data(slopos_ostd::KBox<DataState>),
    /// Connection fully torn down, waiting out `2 × MSL` before the
    /// slot can be reused (RFC 793 §3.5).
    TimeWait(TimeWaitState),
}

impl PcbState {
    /// The next sequence number this side would send, or 0 for a state that
    /// has none. The sequence a RST must carry to be accepted by the peer.
    pub fn snd_nxt_raw(&self) -> u32 {
        match self {
            Self::SynSent(s) => s.snd_nxt.raw(),
            Self::SynRecv(s) => s.snd_nxt.raw(),
            Self::Data(d) => d.snd_nxt.raw(),
            Self::Listen(_) | Self::TimeWait(_) => 0,
        }
    }

    /// Coarse-grained label used by the socket layer to decide which
    /// POSIX state to advertise.
    pub fn observed_socket_state(&self) -> ObservedSocketState {
        match self {
            Self::Listen(_) => ObservedSocketState::Listening,
            Self::SynSent(_) | Self::SynRecv(_) => ObservedSocketState::Connecting,
            Self::Data(d) => match d.close_phase {
                ClosePhase::Established
                | ClosePhase::CloseWait
                | ClosePhase::FinWait1
                | ClosePhase::FinWait2 => ObservedSocketState::Connected,
                ClosePhase::Closing | ClosePhase::LastAck => ObservedSocketState::Closed,
            },
            Self::TimeWait(_) => ObservedSocketState::Closed,
        }
    }

    pub const fn name(&self) -> &'static str {
        match self {
            Self::Listen(_) => "LISTEN",
            Self::SynSent(_) => "SYN_SENT",
            Self::SynRecv(_) => "SYN_RECEIVED",
            Self::Data(_) => "DATA",
            Self::TimeWait(_) => "TIME_WAIT",
        }
    }
}

/// The socket layer's view of a connection's state: `FIN_WAIT_2` and
/// `ESTABLISHED` both look "connected" to `recv()` / `send()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedSocketState {
    Listening,
    Connecting,
    Connected,
    Closed,
}

/// RFC 793 state names, derived on demand from [`PcbState`].
///
/// Never stored — `PcbState` is the sole source of truth. `Closed` is not
/// representable: a released PCB slot is `None`, not a state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpState {
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

impl TcpState {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Listen => "LISTEN",
            Self::SynSent => "SYN_SENT",
            Self::SynReceived => "SYN_RECEIVED",
            Self::Established => "ESTABLISHED",
            Self::FinWait1 => "FIN_WAIT_1",
            Self::FinWait2 => "FIN_WAIT_2",
            Self::CloseWait => "CLOSE_WAIT",
            Self::Closing => "CLOSING",
            Self::LastAck => "LAST_ACK",
            Self::TimeWait => "TIME_WAIT",
        }
    }

    /// Is this state "open" (capable of data transfer or about to be)?
    pub const fn is_open(self) -> bool {
        matches!(
            self,
            Self::Established | Self::FinWait1 | Self::FinWait2 | Self::CloseWait
        )
    }

    pub const fn is_closing(self) -> bool {
        matches!(
            self,
            Self::FinWait1
                | Self::FinWait2
                | Self::CloseWait
                | Self::Closing
                | Self::LastAck
                | Self::TimeWait
        )
    }
}

impl PcbState {
    pub fn tcp_state(&self) -> TcpState {
        match self {
            Self::Listen(_) => TcpState::Listen,
            Self::SynSent(_) => TcpState::SynSent,
            Self::SynRecv(_) => TcpState::SynReceived,
            Self::Data(d) => match d.close_phase {
                ClosePhase::Established => TcpState::Established,
                ClosePhase::FinWait1 => TcpState::FinWait1,
                ClosePhase::FinWait2 => TcpState::FinWait2,
                ClosePhase::CloseWait => TcpState::CloseWait,
                ClosePhase::Closing => TcpState::Closing,
                ClosePhase::LastAck => TcpState::LastAck,
            },
            Self::TimeWait(_) => TcpState::TimeWait,
        }
    }
}
