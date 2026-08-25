//! `SynSent` state: active-open client, waiting for SYN+ACK.
//!
//! Segment arrival at `SYN_SENT` follows RFC 793 §3.4.

use core::mem;

use super::super::actions::{Actions, SocketNotify};
use super::super::header::{
    DEFAULT_MSS, DEFAULT_WINDOW_SIZE, TCP_FLAG_ACK, TcpHeader, parse_tcp_options,
};
use super::super::segment::SegmentBuilder;
use super::super::seq::{SeqNum, seq_gt, seq_le};
use super::data::DataState;
use super::syn_recv::SynRecvState;
use super::{Pcb, PcbState};
use crate::timer::TimerToken;

#[derive(Debug)]
pub struct SynSentState {
    pub iss: SeqNum,
    pub snd_nxt: SeqNum,
    pub snd_wnd: u32,
    pub rcv_wnd: u16,
    pub our_wscale: u8,
    pub peer_mss: u16,
    pub rto_ms: u32,
    pub retransmits: u8,
    pub retransmit_token: Option<TimerToken>,
}

impl SynSentState {
    /// The caller must emit the SYN segment; this only populates the state
    /// tracking the half-open handshake.
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

    /// Returns `Actions` by value, not `Result<Actions, _>` — see
    /// `SynRecvState::on_segment` (frame size).
    pub fn on_segment(pcb: &mut Pcb, hdr: &TcpHeader, options: &[u8], now_ms: u64) -> Actions {
        let mut actions = Actions::new();

        // Take `tuple` before borrowing `pcb.state` mutably; both at once is a
        // double borrow.
        let tuple = pcb.tuple;
        let PcbState::SynSent(s) = &mut pcb.state else {
            unreachable!("SynSentState::on_segment called with non-SynSent state");
        };
        let iss = s.iss;
        let snd_nxt = s.snd_nxt;

        if hdr.is_ack() {
            let ack = SeqNum::new(hdr.ack_num);
            if seq_le(hdr.ack_num, iss.raw()) || seq_gt(hdr.ack_num, snd_nxt.raw()) {
                if hdr.is_rst() {
                    return actions;
                }
                actions.push_segment(SegmentBuilder::bare_rst(tuple, hdr.ack_num));
                return actions;
            }
            let _ = ack;
        }

        if hdr.is_rst() {
            if hdr.is_ack() {
                actions.release = true;
                actions.notify |= SocketNotify::RESET_RECEIVED;
            }
            return actions;
        }

        if !hdr.is_syn() {
            return actions;
        }

        let opts = parse_tcp_options(options);
        let peer_mss = opts.mss.unwrap_or(DEFAULT_MSS);
        let irs = SeqNum::new(hdr.seq_num);
        let rcv_nxt = irs.wrapping_add(1);

        let ack_valid_for_our_syn = hdr.is_ack() && seq_gt(hdr.ack_num, iss.raw());
        let snd_una = if ack_valid_for_our_syn {
            SeqNum::new(hdr.ack_num)
        } else {
            iss
        };
        let (snd_wscale, wscale_enabled, snd_wnd) = if let Some(shift) = opts.window_scale {
            (shift, true, (hdr.window_size as u32) << shift)
        } else {
            (0, false, hdr.window_size as u32)
        };
        let our_wscale = if wscale_enabled { s.our_wscale } else { 0 };

        // Every path that leaves the PCB in `SynSent` has already returned, so
        // from here the state is replaced either way and the SYN's own
        // retransmit timer has no segment left to send. Left armed, it fires a
        // data retransmit against a PCB whose send buffer is empty.
        let syn_retransmit = s.retransmit_token.take();

        // Drop the &mut borrow of pcb.state before writing it back.
        let _ = s;

        if let Some(token) = syn_retransmit {
            actions.push_timer(super::super::actions::TimerOp::Cancel { token });
        }

        if ack_valid_for_our_syn {
            let ts_enabled = opts.timestamp.is_some();
            // TODO(tech-debt): allocation failure panics here — it should
            // surface as `TcpError::OutOfMemory` through `tcp::input`.
            let mut data = slopos_ostd::KBox::try_init(DataState::init_new(
                iss,
                irs,
                snd_una,
                snd_nxt, // snd_nxt carries over unchanged (SYN's +1 already counted)
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
            data.sack_permitted = opts.sack_permitted;
            let peer_tsval = opts.timestamp.map(|(tsval, _)| tsval).unwrap_or(0);
            if ts_enabled {
                data.ts_recent = peer_tsval;
            }
            let _old = mem::replace(&mut pcb.state, PcbState::Data(data));

            let mut ack_seg =
                SegmentBuilder::ack(tuple, snd_nxt.raw(), rcv_nxt.raw(), DEFAULT_WINDOW_SIZE);
            if ts_enabled {
                ack_seg.timestamp = Some((now_ms as u32, peer_tsval));
            }
            actions.push_segment(ack_seg);
            actions.notify |= SocketNotify::NEW_ESTABLISHED | SocketNotify::SEND_WAKE;
        } else {
            // Simultaneous open: SYN without ACK → SYN_RECEIVED.
            let ts_offered = opts.timestamp.is_some();
            let mut syn_recv = SynRecvState::new(iss, irs);
            syn_recv.snd_nxt = snd_nxt; // preserve our SYN's +1 advance
            syn_recv.snd_una = iss;
            syn_recv.snd_wnd = snd_wnd;
            syn_recv.peer_mss = peer_mss;
            syn_recv.snd_wscale = snd_wscale;
            syn_recv.wscale_enabled = wscale_enabled;
            syn_recv.our_wscale = our_wscale;
            syn_recv.ts_enabled = ts_offered;
            if let Some((tsval, _)) = opts.timestamp {
                syn_recv.peer_tsval = tsval;
            }
            let _old = mem::replace(&mut pcb.state, PcbState::SynRecv(syn_recv));

            let mut syn_ack_seg = SegmentBuilder::syn_ack(
                tuple,
                iss.raw(),
                rcv_nxt.raw(),
                DEFAULT_WINDOW_SIZE,
                DEFAULT_MSS,
            );
            if ts_offered {
                let peer_ts = opts.timestamp.map(|(v, _)| v).unwrap_or(0);
                syn_ack_seg.timestamp = Some((now_ms as u32, peer_ts));
            }
            actions.push_segment(syn_ack_seg);
        }
        // Silences an unused warning for `TCP_FLAG_ACK`, used only via the
        // builder.
        let _ = TCP_FLAG_ACK;

        actions
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_assert_invariants(&self, _pcb: &Pcb) {
        debug_assert_eq!(
            self.snd_nxt,
            self.iss.wrapping_add(1),
            "SynSent: snd_nxt == iss + 1"
        );
        debug_assert!(
            self.rcv_wnd as usize <= super::super::buffer::TCP_BUFFER_SIZE,
            "rcv_wnd exceeds buffer"
        );
    }
}
