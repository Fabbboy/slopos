//! Process control block — the state machine each TCP connection lives
//! inside.
//!
//! Every active connection owns one [`Pcb`].  The `Pcb` struct is a flat
//! container with a small common header ([`TcpTuple`], socket back-pointer,
//! send/recv buffers) plus a variant-sized [`PcbState`] payload that
//! carries only the fields a given state needs.  The state machine
//! transitions it via `&mut self` — the smoltcp/h2 pattern — so the
//! containing `PcbTable` can keep each `Pcb` in place inside a
//! `[Option<Pcb>; MAX_CONNECTIONS]` slot without any `take()`/put-back
//! dance.
//!
//! Each per-state handler ([`ListenState::on_segment`] etc.) lives in
//! its own submodule with a dedicated test suite.  This module declares
//! the types and the top-level `Pcb::on_segment` dispatcher that routes
//! a decoded segment to the matching handler.
//!
//! # Why `&mut self` and not consume-by-value
//!
//! A consume-self signature (`fn on_segment(self, ...) -> Pcb`) is the
//! textbook idiom for standalone state machines, but `Pcb` lives inside
//! `[Option<Pcb>; MAX_CONNECTIONS]` behind an `IrqMutex`.  Consuming
//! self would require a `take()`/transform/`put_back` dance at every
//! transition site inside the state handlers, for zero additional
//! type-safety.  Both `smoltcp` (`smoltcp::socket::tcp::Socket`) and
//! `h2` (`h2::proto::streams::stream::StreamState`) resolve this the
//! same way we do: mutate in place via `self.state = PcbState::...`.

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

// -----------------------------------------------------------------------------
// SocketId — stand-in newtype for the socket-layer back-pointer
// -----------------------------------------------------------------------------

/// Opaque handle the PCB uses to refer back to its owning socket.
///
/// The socket layer continues to identify sockets by `usize` internally;
/// this newtype is a type-safe wrapper used only where the PCB needs to
/// record "which socket asked for me".  Zero-cost at runtime
/// (`#[repr(transparent)]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SocketId(pub u32);

// -----------------------------------------------------------------------------
// Pcb — the flat container
// -----------------------------------------------------------------------------

/// Process control block.  One of these per active TCP connection.
///
/// Stored inside `[Option<Pcb>; MAX_CONNECTIONS]` behind
/// `IrqMutex<PcbTable>` (see [`crate::tcp::table`]).  State transitions
/// mutate `self.state` in place; the glue layer in `tcp::input`
/// drains the returned [`Actions`] after the table lock is released.
#[derive(Debug)]
pub struct Pcb {
    /// The four-tuple identifying this connection.
    pub tuple: TcpTuple,

    /// Back-pointer to the socket that owns this PCB, when one exists.
    /// `None` for anonymous server-side children before `accept()`.
    pub socket_id: Option<SocketId>,

    /// State-specific payload.  See [`PcbState`].
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

    /// Apply a decoded incoming segment to this PCB.  Mutates `self`
    /// in place and returns the [`Actions`] the glue layer should
    /// apply after the table lock is dropped.
    ///
    /// This is the single dispatch point for incoming segments — every
    /// state's handler routes through here.  Invariants are checked
    /// before and after the transition in debug builds; release builds
    /// compile the check away.
    pub fn on_segment(
        &mut self,
        bufs: &mut TcpBufferPair,
        hdr: &TcpHeader,
        options: &[u8],
        payload: &[u8],
        now_ms: u64,
    ) -> Actions {
        self.assert_invariants(bufs);
        let actions = match &mut self.state {
            PcbState::Listen(_) => listen::ListenState::on_segment(self, hdr, options, now_ms),
            PcbState::SynSent(_) => syn_sent::SynSentState::on_segment(self, hdr, options, now_ms),
            PcbState::SynRecv(_) => syn_recv::SynRecvState::on_segment(self, hdr, now_ms),
            PcbState::Data(_) => {
                data::DataState::on_segment(self, bufs, hdr, options, payload, now_ms)
            }
            PcbState::TimeWait(_) => time_wait::TimeWaitState::on_segment(self, hdr, now_ms),
        };
        self.assert_invariants(bufs);
        actions
    }

    /// Debug-only invariant audit.  Zero cost in release builds.
    ///
    /// Each `PcbState` variant has its own constraints (Listen carries
    /// no send buffer, TimeWait carries no data, `DataState`'s
    /// `snd_una <= snd_nxt` must hold, etc.).  The check is called
    /// before and after every state transition so a bad mutation trips
    /// the assertion *at the transition site* rather than at some
    /// unrelated later read.
    #[inline]
    pub fn assert_invariants(&self, bufs: &TcpBufferPair) {
        let _ = bufs;
        #[cfg(debug_assertions)]
        match &self.state {
            PcbState::Listen(s) => s.debug_assert_invariants(bufs),
            PcbState::SynSent(s) => s.debug_assert_invariants(self),
            PcbState::SynRecv(s) => s.debug_assert_invariants(self),
            PcbState::Data(s) => s.debug_assert_invariants(self),
            PcbState::TimeWait(s) => s.debug_assert_invariants(bufs),
        }
    }
}

// -----------------------------------------------------------------------------
// PcbState — enum of per-state structs
// -----------------------------------------------------------------------------

/// Per-state payload — the five variants correspond roughly to the
/// RFC 793 state diagram, but with the four closing substates
/// (`FIN_WAIT_1`, `FIN_WAIT_2`, `CLOSING`, `LAST_ACK`) folded into a
/// single [`DataState`] variant via the [`data::ClosePhase`] sub-enum.
/// This is the right split because the closing substates share every
/// field a `DataState` uses — they only differ in which FIN flags
/// have been exchanged.
#[derive(Debug)]
pub enum PcbState {
    /// Passive open.  The connection is listening for incoming SYNs.
    /// Child PCBs in `SYN_RECEIVED` state are tracked by the separate
    /// `tcp::listener::TcpListenState` two-queue model, not inside
    /// this enum.
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
    Data(DataState),
    /// Connection fully torn down, waiting out `2 × MSL` before the
    /// slot can be reused (RFC 793 §3.5).
    TimeWait(TimeWaitState),
}

impl PcbState {
    /// Coarse-grained label used by the socket layer to decide which
    /// POSIX state to advertise.  Replaces the old `sync_socket_state`
    /// polling loop.
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

    /// Short name for logs / test failures.
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

/// The socket layer's view of a connection's state, decoupled from
/// RFC 793's internal names so the socket layer doesn't need to know
/// whether a TCP connection is in `FIN_WAIT_2` vs `ESTABLISHED` — both
/// look "connected" to `recv()` / `send()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedSocketState {
    Listening,
    Connecting,
    Connected,
    Closed,
}

// -----------------------------------------------------------------------------
// TcpState — RFC 793 names as a derived read-only view
// -----------------------------------------------------------------------------

/// RFC 793 state names, derived on demand from [`PcbState`].
///
/// This enum is **never stored** — `PcbState` is the sole source of
/// truth.  `TcpState` exists so that the socket layer and tests can
/// query "is this connection in ESTABLISHED?" without knowing the
/// internal `DataState` / `ClosePhase` split.  It is produced by
/// [`PcbState::tcp_state()`] and consumed by `tcp_get_state()`.
///
/// `Closed` is not representable: a released PCB slot is `None`, not
/// a state.  Code that previously matched `TcpState::Closed` should
/// match on the slot being missing (`tcp_get_state() == None`).
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
    /// Human-readable name for logging.
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

    /// Is this state a closing/teardown state?
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
    /// Derive the RFC 793 state name from this PCB's current state.
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
