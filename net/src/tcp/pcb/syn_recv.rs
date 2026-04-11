//! `SynReceived` state: server saw a SYN, sent SYN+ACK, waiting for ACK.

use super::super::actions::Actions;
use super::super::header::{DEFAULT_MSS, DEFAULT_WINDOW_SIZE, TcpHeader};
use super::super::seq::SeqNum;
use super::Pcb;
use crate::timer::TimerToken;

/// State-specific payload for `SYN_RECEIVED`.
#[derive(Debug)]
pub struct SynRecvState {
    /// Our initial send sequence number (sent in SYN+ACK).
    pub iss: SeqNum,
    /// Peer's initial receive sequence number (from their SYN).
    pub irs: SeqNum,
    /// Send unacknowledged.  Equal to `iss` until the handshake's
    /// final ACK advances it to `iss + 1`.
    pub snd_una: SeqNum,
    /// Next sequence number we will send.  Equal to `iss + 1`.
    pub snd_nxt: SeqNum,
    /// Next byte we expect to receive.  Equal to `irs + 1`.
    pub rcv_nxt: SeqNum,
    /// Peer's advertised window (parsed from the incoming SYN).
    pub snd_wnd: u32,
    /// Our advertised receive window.
    pub rcv_wnd: u16,
    /// Our receive-side window scale.
    pub our_wscale: u8,
    /// Peer's send-side window scale (from their SYN options).
    pub snd_wscale: u8,
    /// Whether window scaling was negotiated.
    pub wscale_enabled: bool,
    /// Peer MSS parsed from SYN options, or `DEFAULT_MSS`.
    pub peer_mss: u16,
    /// Current retransmission timeout in milliseconds.
    pub rto_ms: u32,
    /// SYN+ACK retransmission counter.
    pub retransmits: u8,
    /// Timer token for pending SYN+ACK retransmit.
    pub retransmit_token: Option<TimerToken>,
}

impl SynRecvState {
    /// Create a fresh SYN_RECEIVED payload after a valid SYN arrives
    /// on a listening socket.
    pub const fn new(iss: SeqNum, irs: SeqNum) -> Self {
        Self {
            iss,
            irs,
            snd_una: iss,
            snd_nxt: iss.wrapping_add(1),
            rcv_nxt: irs.wrapping_add(1),
            snd_wnd: 0,
            rcv_wnd: DEFAULT_WINDOW_SIZE,
            our_wscale: 0,
            snd_wscale: 0,
            wscale_enabled: false,
            peer_mss: DEFAULT_MSS,
            rto_ms: 1000,
            retransmits: 0,
            retransmit_token: None,
        }
    }

    /// Apply an incoming segment to a SYN_RECEIVED PCB.  Real body lands in B.3.
    pub fn on_segment(_pcb: &mut Pcb, _hdr: &TcpHeader, _now_ms: u64) -> Actions {
        Actions::new()
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_assert_invariants(&self, _pcb: &Pcb) {
        debug_assert_eq!(
            self.snd_nxt,
            self.iss.wrapping_add(1),
            "SynRecv: snd_nxt == iss + 1"
        );
        debug_assert_eq!(
            self.rcv_nxt,
            self.irs.wrapping_add(1),
            "SynRecv: rcv_nxt == irs + 1"
        );
        // `snd_una` can be `iss` (awaiting final ACK) or `iss+1`
        // (ACKed at the moment we transition out).
        debug_assert!(
            self.snd_una == self.iss || self.snd_una == self.iss.wrapping_add(1),
            "SynRecv: snd_una in {{iss, iss+1}}"
        );
    }
}
