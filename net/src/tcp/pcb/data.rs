//! `Data` state: active data transfer and the closing-phase chain.
//!
//! Covers every RFC 793 substate that shares a send window, receive
//! window, RTT estimator, and congestion controller — that is,
//! `ESTABLISHED`, `FIN_WAIT_1`, `FIN_WAIT_2`, `CLOSE_WAIT`, `CLOSING`,
//! and `LAST_ACK`.  Which of those six labels currently applies is
//! captured by the [`ClosePhase`] sub-enum on [`DataState`].
//!
//! B.4 ports the full `process_established_and_closing` megafunction
//! into [`DataState::on_segment`], broken into five sub-methods:
//!
//! - `process_ack` — updates `snd_una`, drives `cc.on_ack`, samples RTT,
//!   pops newly-acked entries off `retx`, reschedules the RTO timer
//! - `process_payload` — accepts in-order bytes into `buffers.recv`,
//!   queues OOO segments, drains contiguous from `buffers.ooo`
//! - `process_fin_and_close_phase` — advances [`ClosePhase`] in
//!   response to the peer's FIN flag
//! - `emit_ack_if_needed` — decides whether to emit an ACK now or
//!   defer via the delayed-ACK timer
//! - `on_rst` / `on_unexpected_syn` — teardown helpers

use super::super::actions::Actions;
use super::super::cong::CcAlgo;
use super::super::header::{DEFAULT_MSS, TcpHeader};
use super::super::retx::RetxQueue;
use super::super::rtt::RttEstimator;
use super::super::seq::SeqNum;
use super::Pcb;
use crate::timer::TimerToken;

/// Which RFC 793 closing substate the [`DataState`] is currently in.
///
/// The six variants share the same buffers, RTT, and CC state, but
/// differ in which FIN flags have been exchanged and therefore in
/// what the next FIN/ACK means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosePhase {
    /// Both halves open.  Normal data transfer.
    Established,
    /// Our application called `close()` / `shutdown(SHUT_WR)`; we sent
    /// FIN and are waiting for it to be acknowledged.
    FinWait1,
    /// Our FIN was acknowledged; waiting for the peer to send its FIN.
    FinWait2,
    /// The peer sent FIN before we did.  We're still free to send
    /// data; local close must be initiated explicitly.
    CloseWait,
    /// Simultaneous close: we sent FIN (→FinWait1), then received FIN
    /// before our FIN was acknowledged.  Waiting for both ACKs.
    Closing,
    /// We've acknowledged the peer's FIN and our own FIN is unacked;
    /// the final ACK from the peer will release the slot.
    LastAck,
}

/// State-specific payload for the Data variant.
#[derive(Debug)]
pub struct DataState {
    // -------- Sequence variables (RFC 793 §3.2) --------
    pub iss: SeqNum,
    pub irs: SeqNum,
    pub snd_una: SeqNum,
    pub snd_nxt: SeqNum,
    pub snd_wnd: u32,
    pub rcv_nxt: SeqNum,
    pub rcv_wnd: u16,

    // -------- Options negotiated during handshake --------
    pub peer_mss: u16,
    pub rcv_wscale: u8,
    pub snd_wscale: u8,
    pub wscale_enabled: bool,
    /// Whether the peer signalled SACK permitted during the handshake.
    /// Wired into the actual SACK logic in D.2.
    pub sack_permitted: bool,
    /// Whether Nagle's algorithm is currently active (default `true`;
    /// disabled by `setsockopt(TCP_NODELAY)` in D.4).
    pub nagle_enabled: bool,

    // -------- Close-phase discriminator --------
    pub close_phase: ClosePhase,

    // -------- Smoothed RTT + congestion control --------
    pub rtt: RttEstimator,
    pub cc: CcAlgo,

    // -------- Retransmission --------
    pub retx: RetxQueue,
    pub retransmit_token: Option<TimerToken>,
    /// Duplicate-ACK counter for fast retransmit (RFC 5681 §3.2).
    /// Wired to actually trigger fast retransmit in D.1.
    pub dup_ack_count: u8,

    // -------- Keepalive --------
    pub keepalive_token: Option<TimerToken>,
    pub keepalive_probes_sent: u8,
    pub last_activity_tick: u64,

    // -------- Miscellaneous --------
    pub reset_received: bool,
    /// Set when the peer's FIN has already moved `close_phase` past
    /// `Established`; used to gate further data accepts.
    pub peer_closed: bool,
}

impl DataState {
    /// Create a fresh `DataState` for a newly-established connection.
    /// Fields not passed as arguments are defaulted — in particular
    /// `rtt` and `cc` use their `Default` impls (initial RTO 1s,
    /// NewReno with MSS from `peer_mss`).
    pub fn new(
        iss: SeqNum,
        irs: SeqNum,
        snd_una: SeqNum,
        snd_nxt: SeqNum,
        rcv_nxt: SeqNum,
        snd_wnd: u32,
        rcv_wnd: u16,
        peer_mss: u16,
        snd_wscale: u8,
        rcv_wscale: u8,
        wscale_enabled: bool,
    ) -> Self {
        Self {
            iss,
            irs,
            snd_una,
            snd_nxt,
            snd_wnd,
            rcv_nxt,
            rcv_wnd,
            peer_mss,
            rcv_wscale,
            snd_wscale,
            wscale_enabled,
            sack_permitted: false,
            nagle_enabled: true,
            close_phase: ClosePhase::Established,
            rtt: RttEstimator::new(),
            cc: CcAlgo::new_reno(peer_mss.max(DEFAULT_MSS) as u32),
            retx: RetxQueue::new(),
            retransmit_token: None,
            dup_ack_count: 0,
            keepalive_token: None,
            keepalive_probes_sent: 0,
            last_activity_tick: 0,
            reset_received: false,
            peer_closed: false,
        }
    }

    /// Apply an incoming segment to a Data PCB.  Real body lands in B.4.
    pub fn on_segment(_pcb: &mut Pcb, _hdr: &TcpHeader, _payload: &[u8], _now_ms: u64) -> Actions {
        Actions::new()
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_assert_invariants(&self, _pcb: &Pcb) {
        // snd_una <= snd_nxt must hold in wrapping sense.
        debug_assert!(
            self.snd_una <= self.snd_nxt,
            "Data: snd_una ({}) > snd_nxt ({})",
            self.snd_una.raw(),
            self.snd_nxt.raw()
        );
        // Close-phase-specific invariants.
        match self.close_phase {
            ClosePhase::Established => {
                // Nothing specific — this is the "no FIN yet" baseline.
            }
            ClosePhase::FinWait1 | ClosePhase::FinWait2 => {
                debug_assert!(self.snd_nxt >= self.snd_una, "FinWait: snd_nxt >= snd_una");
            }
            ClosePhase::CloseWait => {
                debug_assert!(self.peer_closed, "CloseWait implies peer_closed");
            }
            ClosePhase::Closing | ClosePhase::LastAck => {
                debug_assert!(self.peer_closed, "Closing/LastAck implies peer_closed");
            }
        }
        // `retx.inflight_bytes == snd_nxt - snd_una` is the big
        // invariant that P4.2 introduced.  Left here because it's the
        // one assertion that would catch a bug silently corrupting the
        // inflight counter.
        let expected_inflight = self.snd_nxt.raw().wrapping_sub(self.snd_una.raw());
        debug_assert_eq!(
            self.retx.inflight_bytes(),
            expected_inflight,
            "Data: retx.inflight_bytes ({}) != snd_nxt - snd_una ({})",
            self.retx.inflight_bytes(),
            expected_inflight,
        );
    }
}
