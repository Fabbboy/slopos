//! `SynSent` state: active-open client, waiting for SYN+ACK.
//!
//! RFC 793 §3.4 segment arrival at `SYN_SENT` has four cases:
//!
//! 1. **ACK with bad value** → `RST(seq = ack_num)` unless the
//!    incoming segment is already a RST (in which case silently
//!    drop).  "Bad value" means `ack_num <= iss` or
//!    `ack_num > snd_nxt`; a valid SYN must ack our SYN.
//! 2. **RST + valid ACK** → connection refused; release the PCB and
//!    surface `RESET_RECEIVED` to the socket layer.
//! 3. **RST without ACK** → silently drop (unacknowledged RSTs at
//!    `SYN_SENT` are invalid).
//! 4. **SYN** → parse options, update `rcv_nxt`/`irs`/`peer_mss`/
//!    `wscale`.  If the SYN carried a valid ACK, transition to
//!    `Data`/`ClosePhase::Established` and emit a plain ACK;
//!    otherwise (simultaneous open) transition to `SynRecv` and
//!    emit a SYN+ACK.

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

/// State-specific payload for `SYN_SENT`.
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
    /// Create a fresh SYN_SENT payload.  The caller is responsible
    /// for actually emitting the SYN segment; this constructor only
    /// populates the state that tracks the half-open handshake.
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

    /// Apply an incoming segment to a SYN_SENT PCB.
    pub fn on_segment(pcb: &mut Pcb, hdr: &TcpHeader, options: &[u8], _now_ms: u64) -> Actions {
        let mut actions = Actions::new();

        // Snapshot the bits we need — destructuring &mut pcb.state and
        // also borrowing pcb.tuple would be a double borrow, so grab
        // the tuple first and the state borrows second.
        let tuple = pcb.tuple;
        let PcbState::SynSent(s) = &mut pcb.state else {
            unreachable!("SynSentState::on_segment called with non-SynSent state");
        };
        let iss = s.iss;
        let snd_nxt = s.snd_nxt;

        // Step 1: if the segment has an ACK flag, verify the ack_num
        // is within the valid range (iss, snd_nxt].  A bad ACK gets a
        // RST reply (unless the stranger already sent RST, in which
        // case we drop silently).
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

        // Step 2: RST handling.  A RST with a valid ACK (verified in
        // step 1) means the peer refused the connection.  Mark the
        // PCB for release and surface the reset to the socket layer.
        if hdr.is_rst() {
            if hdr.is_ack() {
                actions.release = true;
                actions.notify |= SocketNotify::RESET_RECEIVED;
            }
            return actions;
        }

        // Step 3: we only care about SYNs from here on.
        if !hdr.is_syn() {
            return actions;
        }

        // Step 4: parse options and pull everything we need out of the
        // incoming header before we swap out pcb.state.
        let opts = parse_tcp_options(options);
        let peer_mss = opts.mss.unwrap_or(DEFAULT_MSS);
        let irs = SeqNum::new(hdr.seq_num);
        let rcv_nxt = irs.wrapping_add(1);

        // Work out the final snd_una / snd_wnd based on whether this
        // segment also acknowledges our SYN.
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

        // Drop the &mut borrow of pcb.state before writing it back.
        let _ = s;

        if ack_valid_for_our_syn {
            // SYN+ACK acknowledging our SYN → ESTABLISHED.
            // Build the new DataState and replace pcb.state.
            let mut data = DataState::new(
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
            );
            data.sack_permitted = opts.sack_permitted;
            let _old = mem::replace(&mut pcb.state, PcbState::Data(data));

            // Emit plain ACK that closes out the 3WHS from our side.
            actions.push_segment(SegmentBuilder::ack(
                tuple,
                snd_nxt.raw(),
                rcv_nxt.raw(),
                DEFAULT_WINDOW_SIZE,
            ));
            actions.notify |= SocketNotify::NEW_ESTABLISHED | SocketNotify::SEND_WAKE;
        } else {
            // Simultaneous open: SYN without ACK → SYN_RECEIVED.
            let mut syn_recv = SynRecvState::new(iss, irs);
            syn_recv.snd_nxt = snd_nxt; // preserve our SYN's +1 advance
            syn_recv.snd_una = iss;
            syn_recv.snd_wnd = snd_wnd;
            syn_recv.peer_mss = peer_mss;
            syn_recv.snd_wscale = snd_wscale;
            syn_recv.wscale_enabled = wscale_enabled;
            syn_recv.our_wscale = our_wscale;
            let _old = mem::replace(&mut pcb.state, PcbState::SynRecv(syn_recv));

            // Emit SYN+ACK that reflects the simultaneous-open
            // crossover.
            actions.push_segment(SegmentBuilder::syn_ack(
                tuple,
                iss.raw(),
                rcv_nxt.raw(),
                DEFAULT_WINDOW_SIZE,
                DEFAULT_MSS,
            ));
        }
        // Silence an unused TCP_FLAG_ACK warning if the compiler can't
        // see it used via the builder.
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
