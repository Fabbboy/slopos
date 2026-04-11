//! `SynSent` state: active-open client, waiting for SYN+ACK.

use super::super::actions::Actions;
use super::super::buffer::TCP_BUFFER_SIZE;
use super::super::header::{DEFAULT_MSS, DEFAULT_WINDOW_SIZE, TcpHeader};
use super::super::seq::SeqNum;
use super::Pcb;
use crate::timer::TimerToken;

/// State-specific payload for `SYN_SENT`.
///
/// All fields are `pub(super)` so the B.2 implementation can populate
/// them directly; external callers use the accessor methods at the
/// bottom of the file.
#[derive(Debug)]
pub struct SynSentState {
    /// Our initial send sequence number (the value we put in the SYN).
    pub iss: SeqNum,
    /// Next sequence number we will send.  Equal to `iss + 1` until
    /// we move to DATA (SYN consumes one sequence number).
    pub snd_nxt: SeqNum,
    /// Peer's advertised send window — populated once we see SYN+ACK.
    pub snd_wnd: u32,
    /// Our advertised receive window (starts at `DEFAULT_WINDOW_SIZE`).
    pub rcv_wnd: u16,
    /// Our receive-side window scale (option exchanged in SYN/SYN-ACK).
    pub our_wscale: u8,
    /// Peer MSS parsed from SYN+ACK, or `DEFAULT_MSS` until known.
    pub peer_mss: u16,
    /// Current retransmission timeout in milliseconds.
    pub rto_ms: u32,
    /// Number of SYN retransmissions performed so far.
    pub retransmits: u8,
    /// Timer token for the SYN retransmission timer (None until armed).
    pub retransmit_token: Option<TimerToken>,
}

impl SynSentState {
    /// Create a fresh SYN_SENT payload for an active-open attempt.
    pub const fn new(iss: SeqNum) -> Self {
        Self {
            iss,
            snd_nxt: iss.wrapping_add(1),
            snd_wnd: 0,
            rcv_wnd: DEFAULT_WINDOW_SIZE,
            our_wscale: 0,
            peer_mss: DEFAULT_MSS,
            rto_ms: 1000,
            retransmits: 0,
            retransmit_token: None,
        }
    }

    /// Apply an incoming segment to a SYN_SENT PCB.  Real body lands in B.2.
    pub fn on_segment(_pcb: &mut Pcb, _hdr: &TcpHeader, _options: &[u8], _now_ms: u64) -> Actions {
        Actions::new()
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_assert_invariants(&self, _pcb: &Pcb) {
        debug_assert_eq!(
            self.snd_nxt,
            self.iss.wrapping_add(1),
            "SynSent: snd_nxt == iss + 1"
        );
        debug_assert!(
            self.rcv_wnd as usize <= TCP_BUFFER_SIZE,
            "rcv_wnd exceeds buffer"
        );
    }
}
