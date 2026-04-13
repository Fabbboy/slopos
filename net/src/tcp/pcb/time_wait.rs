//! `TimeWait` state: connection fully closed, waiting out `2 × MSL`.
//!
//! RFC 793 §3.5: a connection in `TIME_WAIT` ignores most traffic but
//! does two useful things:
//!
//! 1. **Retransmitted FIN** → re-ACK and restart the `2 × MSL` timer.
//!    The peer may not have seen our previous ACK, so we issue it
//!    again without transitioning out of TIME_WAIT.
//! 2. **RST** → release the slot immediately (no reason to wait out
//!    MSL when the peer's already given up).
//!
//! Everything else is dropped silently.  TimeWait carries no send or
//! receive buffers — those were drained on entry — so there's nothing
//! useful to do with data or other flags.

use super::super::actions::{Actions, SocketNotify};
use super::super::challenge_ack;
use super::super::header::TcpHeader;
use super::super::segment::SegmentBuilder;
use super::super::seq::SeqNum;
use super::{Pcb, PcbState};
use crate::timer::TimerToken;

/// `2 × MSL` in milliseconds.  MSL = 30 s per RFC 793 §3.3.
pub const TIME_WAIT_MS: u64 = 60_000;

/// State-specific payload for `TIME_WAIT`.
#[derive(Debug)]
pub struct TimeWaitState {
    pub last_rcv_nxt: SeqNum,
    pub last_snd_nxt: SeqNum,
    pub last_rcv_wnd: u16,
    pub entry_ms: u64,
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

    /// Apply an incoming segment to a TimeWait PCB.
    pub fn on_segment(pcb: &mut Pcb, hdr: &TcpHeader, now_ms: u64) -> Actions {
        let mut actions = Actions::new();

        let tuple = pcb.tuple;
        let PcbState::TimeWait(s) = &mut pcb.state else {
            unreachable!("TimeWaitState::on_segment called with non-TimeWait state");
        };

        // RST — RFC 5961: validate sequence against frozen window.
        // In TIME_WAIT, any in-window RST releases the slot early.
        // No challenge ACK — the connection is already closing.
        if hdr.is_rst() {
            let effective_wnd = s.last_rcv_wnd as u32;
            match challenge_ack::classify_rst(hdr.seq_num, s.last_rcv_nxt.raw(), effective_wnd) {
                challenge_ack::RstAction::Accept | challenge_ack::RstAction::ChallengeAck => {
                    actions.release = true;
                    actions.notify |= SocketNotify::RESET_RECEIVED;
                    return actions;
                }
                challenge_ack::RstAction::Drop => {
                    return actions;
                }
            }
        }

        // Retransmitted FIN → re-ACK with the frozen-in-amber sequence
        // numbers and restart the MSL timer.
        if hdr.is_fin() {
            actions.push_segment(SegmentBuilder::ack(
                tuple,
                s.last_snd_nxt.raw(),
                s.last_rcv_nxt.raw(),
                s.last_rcv_wnd,
            ));
            s.entry_ms = now_ms;
            // The caller (glue layer) cancels the old timer token
            // and schedules a new one based on `s.entry_ms`.  We
            // can't do that inline because Actions doesn't carry
            // tokens yet — the D.5 cleanup pulls this tidy-up into
            // the glue layer.
        }

        // Everything else → drop silently.
        actions
    }

    // TimeWait-specific invariants are now checked by
    // `Pcb::assert_invariants` (buffer_id must be None).  No
    // per-state debug hook needed.
}
