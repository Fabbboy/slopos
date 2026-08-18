//! `TimeWait` state: connection fully closed, waiting out `2 × MSL`
//! (RFC 793 §3.5).
//!
//! A retransmitted FIN is re-ACKed without leaving TIME_WAIT and a RST releases
//! the slot early; everything else is dropped, since the send and receive
//! buffers were drained on entry.

use super::super::actions::{Actions, SocketNotify};
use super::super::challenge_ack;
use super::super::header::TcpHeader;
use super::super::segment::SegmentBuilder;
use super::super::seq::SeqNum;
use super::{Pcb, PcbState};
use crate::timer::TimerToken;

/// `2 × MSL` in milliseconds.  MSL = 30 s per RFC 793 §3.3.
pub const TIME_WAIT_MS: u64 = 60_000;

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

    pub fn on_segment(pcb: &mut Pcb, hdr: &TcpHeader, now_ms: u64) -> Actions {
        let mut actions = Actions::new();

        let tuple = pcb.tuple;
        let PcbState::TimeWait(s) = &mut pcb.state else {
            unreachable!("TimeWaitState::on_segment called with non-TimeWait state");
        };

        // RST — RFC 5961: validate sequence against the frozen window.
        // Any in-window RST releases the slot early; no challenge ACK is sent.
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

        if hdr.is_fin() {
            actions.push_segment(SegmentBuilder::ack(
                tuple,
                s.last_snd_nxt.raw(),
                s.last_rcv_nxt.raw(),
                s.last_rcv_wnd,
            ));
            s.entry_ms = now_ms;
            // TODO(tech-debt): the MSL timer is not restarted here — `Actions`
            // carries no timer token, so the glue layer must reschedule from
            // `s.entry_ms`.
        }

        actions
    }
}
