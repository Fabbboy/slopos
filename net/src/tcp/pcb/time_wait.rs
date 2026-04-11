//! `TimeWait` state: connection fully closed, waiting out `2 × MSL`.

use super::super::actions::Actions;
use super::super::header::TcpHeader;
use super::super::seq::SeqNum;
use super::Pcb;
use crate::timer::TimerToken;

/// State-specific payload for `TIME_WAIT`.
///
/// A `TimeWait` PCB carries no send or receive buffers — both were
/// drained when the connection transitioned here — only enough state
/// to re-ACK a retransmitted FIN from the peer and to expire itself
/// after `2 × MSL`.
#[derive(Debug)]
pub struct TimeWaitState {
    /// `rcv_nxt` at the moment of entry.  Used as the ACK value in
    /// any re-ACK we emit in response to a retransmitted FIN.
    pub last_rcv_nxt: SeqNum,
    /// `snd_nxt` at the moment of entry.  Stays static; used as the
    /// SEQ value on a re-ACK.
    pub last_snd_nxt: SeqNum,
    /// Our last advertised receive window.  Advertised verbatim on
    /// re-ACKs.
    pub last_rcv_wnd: u16,
    /// Timestamp (`now_ms`) when TIME_WAIT was entered.
    pub entry_ms: u64,
    /// Pending `2 × MSL` expiry timer, when armed.
    pub expire_token: Option<TimerToken>,
}

impl TimeWaitState {
    pub const fn new(
        last_rcv_nxt: SeqNum,
        last_snd_nxt: SeqNum,
        last_rcv_wnd: u16,
        entry_ms: u64,
    ) -> Self {
        Self {
            last_rcv_nxt,
            last_snd_nxt,
            last_rcv_wnd,
            entry_ms,
            expire_token: None,
        }
    }

    /// Apply an incoming segment to a TimeWait PCB.  Real body lands in B.5.
    pub fn on_segment(_pcb: &mut Pcb, _hdr: &TcpHeader, _now_ms: u64) -> Actions {
        Actions::new()
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_assert_invariants(&self, pcb: &Pcb) {
        debug_assert!(
            pcb.buffers.send.buffered_len() == 0,
            "TimeWait must have no send data"
        );
    }
}
