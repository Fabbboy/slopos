//! `SynReceived` state: server saw a SYN, sent SYN+ACK, waiting for ACK.
//!
//! Segment arrival follows RFC 793 §3.4.  Non-ACK segments are dropped
//! silently — a correct 3WHS cannot produce one here.

use core::mem;

use super::super::actions::{Actions, SocketNotify};
use super::super::challenge_ack;
use super::super::header::{DEFAULT_MSS, DEFAULT_WINDOW_SIZE, TcpHeader};
use super::super::segment::SegmentBuilder;
use super::super::seq::{SeqNum, seq_gt, seq_lt};
use super::data::DataState;
use super::{Pcb, PcbState};
use crate::timer::TimerToken;

#[derive(Debug)]
pub struct SynRecvState {
    pub iss: SeqNum,
    pub irs: SeqNum,
    pub snd_una: SeqNum,
    pub snd_nxt: SeqNum,
    pub rcv_nxt: SeqNum,
    pub snd_wnd: u32,
    pub rcv_wnd: u16,
    pub our_wscale: u8,
    pub snd_wscale: u8,
    pub wscale_enabled: bool,
    pub peer_mss: u16,
    pub sack_permitted: bool,
    pub rto_ms: u32,
    pub retransmits: u8,
    pub retransmit_token: Option<TimerToken>,
    pub ts_enabled: bool,
    pub peer_tsval: u32,
}

impl SynRecvState {
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
            sack_permitted: false,
            rto_ms: 1000,
            retransmits: 0,
            retransmit_token: None,
            ts_enabled: false,
            peer_tsval: 0,
        }
    }

    /// Returns `Actions` by value: a `Result` discriminant around the ~1 KiB
    /// `Actions` pushes the per-handler stack frame above the stack-size gate.
    pub fn on_segment(pcb: &mut Pcb, hdr: &TcpHeader, _now_ms: u64) -> Actions {
        let mut actions = Actions::new();

        let tuple = pcb.tuple;
        let PcbState::SynRecv(s) = &mut pcb.state else {
            unreachable!("SynRecvState::on_segment called with non-SynRecv state");
        };

        // RST — RFC 5961: validate sequence against receive window.  Any
        // in-window RST tears down the half-open connection; no challenge ACK,
        // since the connection is not established.
        if hdr.is_rst() {
            let effective_wnd = s.rcv_wnd as u32;
            match challenge_ack::classify_rst(hdr.seq_num, s.rcv_nxt.raw(), effective_wnd) {
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

        if !hdr.is_ack() {
            return actions;
        }

        if seq_lt(hdr.ack_num, s.snd_una.raw()) || seq_gt(hdr.ack_num, s.snd_nxt.raw()) {
            actions.push_segment(SegmentBuilder::bare_rst(tuple, hdr.ack_num));
            return actions;
        }

        // Capture every field we need before the variant is swapped out.
        let iss = s.iss;
        let irs = s.irs;
        let snd_nxt = s.snd_nxt;
        let rcv_nxt = s.rcv_nxt;
        let peer_mss = s.peer_mss;
        let snd_wscale = s.snd_wscale;
        let our_wscale = s.our_wscale;
        let wscale_enabled = s.wscale_enabled;
        let snd_una = SeqNum::new(hdr.ack_num);
        let snd_wnd = if wscale_enabled {
            (hdr.window_size as u32) << snd_wscale
        } else {
            hdr.window_size as u32
        };
        let ts_enabled = s.ts_enabled;
        let _peer_tsval = s.peer_tsval;
        let _ = s;
        // TODO(tech-debt): `.expect` on OOM kills the connection — thread
        // `Result<KBox<Actions>, _>` out once the dispatcher takes an out-param.
        let data = slopos_ostd::KBox::try_init(DataState::init_new(
            iss,
            irs,
            snd_una,
            snd_nxt,
            rcv_nxt,
            snd_wnd,
            DEFAULT_WINDOW_SIZE,
            peer_mss,
            snd_wscale,
            our_wscale,
            wscale_enabled,
            ts_enabled,
        ))
        .expect("DataState alloc failed");
        let _old = mem::replace(&mut pcb.state, PcbState::Data(data));

        actions.notify |= SocketNotify::NEW_ESTABLISHED | SocketNotify::ACCEPT_WAKE;
        // No outgoing segment: the 3WHS is complete.
        let _ = tuple;
        actions
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
        debug_assert!(
            self.snd_una == self.iss || self.snd_una == self.iss.wrapping_add(1),
            "SynRecv: snd_una in {{iss, iss+1}}"
        );
    }
}
