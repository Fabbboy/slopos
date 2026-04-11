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
//! The per-state handlers ([`ListenState::on_segment`] etc.) land in
//! Phase B as separate commits, each with its own unit-test suite.  This
//! module declares the types and the top-level `Pcb::on_segment`
//! dispatcher that routes a decoded segment to the matching handler.
//!
//! # Architecture (Phase A, Phase B, Phase C)
//!
//! - **A (skeleton).**  Types exist but `on_segment` bodies are
//!   `todo!()` / return an empty `Actions`.  No production code routes
//!   through this module yet; the legacy `TcpConnection` path in
//!   `tcp/mod.rs` still owns all real traffic.
//! - **B (per-state ports).**  Each `XxxState::on_segment` gets a real
//!   implementation plus a dedicated `tcp_pcb_xxx_tests` suite.  The
//!   new handlers are tested against synthetic inputs in isolation.
//! - **C (atomic cutover).**  `tcp_input` switches from the legacy
//!   `process_*` chain to `Pcb::on_segment`; `TcpConnection` is
//!   deleted; every caller migrates directly with no compat shim.
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
/// mutate `self.state` in place; the glue layer in `tcp::tcp_input`
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

    /// Send + receive + OOO reassembly buffers.  Allocated for every
    /// PCB (even `Listen`, where they stay empty) so transitions never
    /// need to allocate.
    pub buffers: TcpBufferPair,
}

impl Pcb {
    /// Create a new PCB in the given state.  Buffers are freshly
    /// zero-initialized.
    pub const fn new(tuple: TcpTuple, state: PcbState) -> Self {
        Self {
            tuple,
            socket_id: None,
            state,
            buffers: TcpBufferPair::new(),
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
        hdr: &TcpHeader,
        options: &[u8],
        payload: &[u8],
        now_ms: u64,
    ) -> Actions {
        self.assert_invariants();
        let actions = match &mut self.state {
            PcbState::Listen(_) => listen::ListenState::on_segment(self, hdr, options, now_ms),
            PcbState::SynSent(_) => syn_sent::SynSentState::on_segment(self, hdr, options, now_ms),
            PcbState::SynRecv(_) => syn_recv::SynRecvState::on_segment(self, hdr, now_ms),
            PcbState::Data(_) => data::DataState::on_segment(self, hdr, payload, now_ms),
            PcbState::TimeWait(_) => time_wait::TimeWaitState::on_segment(self, hdr, now_ms),
        };
        self.assert_invariants();
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
    pub fn assert_invariants(&self) {
        #[cfg(debug_assertions)]
        match &self.state {
            PcbState::Listen(s) => s.debug_assert_invariants(self),
            PcbState::SynSent(s) => s.debug_assert_invariants(self),
            PcbState::SynRecv(s) => s.debug_assert_invariants(self),
            PcbState::Data(s) => s.debug_assert_invariants(self),
            PcbState::TimeWait(s) => s.debug_assert_invariants(self),
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
