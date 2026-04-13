//! `SynReceived` state: server saw a SYN, sent SYN+ACK, waiting for ACK.
//!
//! RFC 793 §3.4 segment arrival at `SYN_RECEIVED` has three cases:
//!
//! 1. **RST** → release the PCB and flag `RESET_RECEIVED`.
//! 2. **ACK with bad value** → reply with `RST(seq = ack_num)`.
//!    A "bad" ACK here means `ack_num < snd_una` or
//!    `ack_num > snd_nxt`.
//! 3. **Valid ACK** → transition to `Data` / `ClosePhase::Established`.
//!    The connection is now fully open.
//!
//! Non-ACK segments are dropped silently (they would be impossible
//! in a correct 3WHS — at this point the peer must send ACK).

use core::mem;

use super::super::actions::{Actions, SocketNotify};
use super::super::header::{DEFAULT_MSS, DEFAULT_WINDOW_SIZE, TcpHeader};
use super::super::segment::SegmentBuilder;
use super::super::seq::{SeqNum, seq_gt, seq_lt};
use super::data::DataState;
use super::{Pcb, PcbState};
use crate::timer::TimerToken;

/// State-specific payload for `SYN_RECEIVED`.
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
        }
    }

    /// Apply an incoming segment to a SYN_RECEIVED PCB.
    pub fn on_segment(pcb: &mut Pcb, hdr: &TcpHeader, _now_ms: u64) -> Actions {
        let mut actions = Actions::new();

        let tuple = pcb.tuple;
        let PcbState::SynRecv(s) = &mut pcb.state else {
            unreachable!("SynRecvState::on_segment called with non-SynRecv state");
        };

        // RST → release.
        if hdr.is_rst() {
            actions.release = true;
            actions.notify |= SocketNotify::RESET_RECEIVED;
            return actions;
        }

        // Must have ACK.
        if !hdr.is_ack() {
            return actions;
        }

        // Validate ACK range.
        if seq_lt(hdr.ack_num, s.snd_una.raw()) || seq_gt(hdr.ack_num, s.snd_nxt.raw()) {
            actions.push_segment(SegmentBuilder::bare_rst(tuple, hdr.ack_num));
            return actions;
        }

        // Valid ACK → ESTABLISHED.  Capture every field we need out
        // of the SynRecv state before we swap the variant out.
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
        let _ = s;

        let data = DataState::new(
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
        );
        let _old = mem::replace(&mut pcb.state, PcbState::Data(data));

        actions.notify |= SocketNotify::NEW_ESTABLISHED | SocketNotify::ACCEPT_WAKE;
        // Note: no outgoing segment — the 3WHS is complete; the peer's
        // ACK doesn't need a response.
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
