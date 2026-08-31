//! `SynReceived` state: server saw a SYN, sent SYN+ACK, waiting for ACK.
//!
//! Segment arrival follows RFC 793 §3.4.  Non-ACK segments are dropped
//! silently — a correct 3WHS cannot produce one here.

use core::mem;

use super::super::actions::{Actions, SocketNotify, TimerOp};
use super::super::challenge_ack;
use super::super::header::{DEFAULT_MSS, DEFAULT_WINDOW_SIZE, TcpHeader};
use super::super::segment::{SegmentBuilder, TcpOutSegment};
use super::super::seq::{SeqNum, seq_gt, seq_lt};
use super::super::tuple::TcpTuple;
use super::data::DataState;
use super::{Pcb, PcbState};
use crate::timer::TimerToken;

/// Which open brought the connection into `SYN_RECEIVED`.
///
/// RFC 9293 §3.10.7.4 answers a RST here differently for each: a passive open
/// returns to LISTEN without telling the user, an active one signals
/// "connection refused". The passive variant is carried for that distinction
/// only — its half-open retransmits belong to the listener's `SynQueue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOrigin {
    Passive,
    Simultaneous,
}

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
    pub origin: OpenOrigin,
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
            origin: OpenOrigin::Passive,
        }
    }

    /// The SYN-ACK for this half-open connection, at `iss` rather than
    /// `snd_nxt`: the SYN occupies `iss`, and re-sending it a sequence position
    /// later is a segment the peer cannot match to the handshake it is in.
    pub fn syn_ack(&self, tuple: TcpTuple, now_ms: u64) -> TcpOutSegment {
        let mut seg = SegmentBuilder::syn_ack(
            tuple,
            self.iss.raw(),
            self.rcv_nxt.raw(),
            self.rcv_wnd,
            DEFAULT_MSS,
            self.wscale_enabled.then_some(self.our_wscale),
            self.sack_permitted,
        );
        if self.ts_enabled {
            seg.timestamp = Some((now_ms as u32, self.peer_tsval));
        }
        seg
    }

    /// Returns `Actions` by value: a `Result` discriminant around the ~1 KiB
    /// `Actions` pushes the per-handler stack frame above the stack-size gate.
    pub fn on_segment(pcb: &mut Pcb, hdr: &TcpHeader, _now_ms: u64) -> Actions {
        let mut actions = Actions::new();

        let tuple = pcb.tuple;
        let PcbState::SynRecv(s) = &mut pcb.state else {
            unreachable!("SynRecvState::on_segment called with non-SynRecv state");
        };

        // RFC 5961 §3.2: an in-window RST that is not an exact `rcv_nxt` match
        // gets a challenge ACK, not a teardown. A simultaneous open now lives
        // here for tens of seconds, which is that long a blind-reset window.
        if hdr.is_rst() {
            let effective_wnd = s.rcv_wnd as u32;
            match challenge_ack::classify_rst(hdr.seq_num, s.rcv_nxt.raw(), effective_wnd) {
                challenge_ack::RstAction::Accept => {
                    actions.release = true;
                    actions.notify |= SocketNotify::RESET_RECEIVED;
                    return actions;
                }
                challenge_ack::RstAction::ChallengeAck => {
                    actions.push_segment(SegmentBuilder::ack(
                        tuple,
                        s.snd_nxt.raw(),
                        s.rcv_nxt.raw(),
                        s.rcv_wnd,
                    ));
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

        // The first window that is scaled: the SYN-ACK's was not (RFC 7323 §2.2).
        s.snd_una = SeqNum::new(hdr.ack_num);
        s.snd_wnd = if s.wscale_enabled {
            (hdr.window_size as u32) << s.snd_wscale
        } else {
            hdr.window_size as u32
        };
        s.rcv_wnd = DEFAULT_WINDOW_SIZE;

        // Unowned once the variant is replaced: `cancel_pcb_timers` would no
        // longer find it, and it would fire against the `Data` state's own RTO.
        let handshake_timer = s.retransmit_token.take();

        // TODO(tech-debt): `.expect` on OOM kills the connection — thread
        // `Result<KBox<Actions>, _>` out once the dispatcher takes an out-param.
        let data = slopos_ostd::KBox::try_init(DataState::init_from_syn_recv(s))
            .expect("DataState alloc failed");
        let _old = mem::replace(&mut pcb.state, PcbState::Data(data));

        if let Some(token) = handshake_timer {
            actions.push_timer(TimerOp::Cancel { token });
        }

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
