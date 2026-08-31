//! `Data` state: active data transfer and the closing-phase chain.
//!
//! Covers every RFC 793 substate that shares a send window, receive
//! window, RTT estimator, and congestion controller — that is,
//! `ESTABLISHED`, `FIN_WAIT_1`, `FIN_WAIT_2`, `CLOSE_WAIT`, `CLOSING`,
//! and `LAST_ACK`.  Which of those six labels currently applies is
//! captured by the [`ClosePhase`] sub-enum on [`DataState`].

use core::mem;

use slopos_ostd::{
    AllocError, Init, Initialised, SlotPtr, init_struct_with, write_field, zero_field,
};

use super::super::actions::{Actions, SocketNotify, TimerOp};
use super::super::buffer::TcpBufferPair;
use super::super::challenge_ack;
use super::super::cong::{CcAlgo, CongestionControl};
use super::super::header::{DEFAULT_MSS, TcpHeader};
use super::super::retx::SendMap;
use super::super::rtt::RttEstimator;
use super::super::segment::SegmentBuilder;
use super::super::seq::{SeqNum, seq_gt, seq_le};
use super::time_wait::{TIME_WAIT_MS, TimeWaitState};
use super::{Pcb, PcbState};
use crate::timer::{TimerKind, TimerToken};

/// Which RFC 793 closing substate the [`DataState`] is currently in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosePhase {
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
}

/// How a sub-method signals a variant change or slot release back to the top
/// dispatcher, so it can keep operating on `&mut DataState` without owning the
/// enum swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NextTransition {
    StayInData,
    ToTimeWait,
    ReleaseNow,
}

/// State-specific payload for the Data variant.
#[derive(Debug, slopos_ostd::SlotFields)]
pub struct DataState {
    pub iss: SeqNum,
    pub irs: SeqNum,
    pub snd_una: SeqNum,
    pub snd_nxt: SeqNum,
    pub snd_wnd: u32,
    pub rcv_nxt: SeqNum,
    pub rcv_wnd: u16,

    pub peer_mss: u16,
    pub rcv_wscale: u8,
    pub snd_wscale: u8,
    pub wscale_enabled: bool,
    pub sack_permitted: bool,
    pub nagle_enabled: bool,

    pub close_phase: ClosePhase,

    pub rtt: RttEstimator,
    pub cc: CcAlgo,

    pub sendmap: SendMap,
    pub retransmit_token: Option<TimerToken>,

    pub keepalive_token: Option<TimerToken>,
    pub keepalive_probes_sent: u8,
    pub last_activity_tick: u64,

    pub fin_wait2_token: Option<TimerToken>,

    pub ts_enabled: bool,
    pub ts_recent: u32,
    pub last_ack_sent: u32,

    pub reset_received: bool,
    pub peer_closed: bool,

    pub challenge_budget: challenge_ack::ChallengeBudget,
}

impl DataState {
    /// Heap-direct initialiser for a freshly-established `DataState`.
    ///
    /// Returns an [`Init`] recipe rather than a `Self` rvalue so the 3 KiB
    /// struct never materialises on the caller's stack, and is hand-written
    /// rather than macro-expanded so the closure's own frame stays inside the
    /// stack-safety gate.
    #[allow(clippy::too_many_arguments)]
    pub fn init_new(
        iss: SeqNum,
        irs: SeqNum,
        snd_una: SeqNum,
        snd_nxt: SeqNum,
        rcv_nxt: SeqNum,
        snd_wnd: u32,
        rcv_wnd: u16,
        peer_mss: u16,
        snd_wscale: u8,
        rcv_wscale: u8,
        wscale_enabled: bool,
        ts_enabled: bool,
    ) -> impl Init<Self, AllocError> {
        let cc_mss = peer_mss.max(DEFAULT_MSS) as u32;
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, iss, iss);
                write_field!(slot, irs, irs);
                write_field!(slot, snd_una, snd_una);
                write_field!(slot, snd_nxt, snd_nxt);
                write_field!(slot, snd_wnd, snd_wnd);
                write_field!(slot, rcv_nxt, rcv_nxt);
                write_field!(slot, rcv_wnd, rcv_wnd);
                write_field!(slot, peer_mss, peer_mss);
                write_field!(slot, rcv_wscale, rcv_wscale);
                write_field!(slot, snd_wscale, snd_wscale);
                write_field!(slot, wscale_enabled, wscale_enabled);
                write_field!(slot, sack_permitted, false);
                write_field!(slot, nagle_enabled, true);
                write_field!(slot, close_phase, ClosePhase::Established);
                write_field!(slot, rtt, RttEstimator::new());
                write_field!(slot, cc, CcAlgo::cubic(cc_mss));
                // `SendMap` is all-zero-valid (see `SendMap::zero_init_slot`).
                zero_field!(slot, sendmap);
                write_field!(slot, retransmit_token, None);
                write_field!(slot, keepalive_token, None);
                write_field!(slot, keepalive_probes_sent, 0u8);
                write_field!(slot, last_activity_tick, 0u64);
                write_field!(slot, fin_wait2_token, None);
                write_field!(slot, ts_enabled, ts_enabled);
                write_field!(slot, ts_recent, 0u32);
                write_field!(slot, last_ack_sent, 0u32);
                write_field!(slot, reset_received, false);
                write_field!(slot, peer_closed, false);
                write_field!(
                    slot,
                    challenge_budget,
                    challenge_ack::ChallengeBudget::new()
                );
                Ok(slot.finish())
            },
        )
    }

    /// Heap-direct initialiser for the `SYN_RECV → ESTABLISHED` transition;
    /// builds in place so the 3 KiB struct never lands on a caller's stack.
    pub fn init_from_syn_recv(
        s: &super::syn_recv::SynRecvState,
    ) -> impl Init<Self, AllocError> + '_ {
        let cc_mss = s.peer_mss.max(DEFAULT_MSS) as u32;
        let ts_recent = if s.ts_enabled { s.peer_tsval } else { 0 };
        let iss = s.iss;
        let irs = s.irs;
        let snd_una = s.snd_una;
        let snd_nxt = s.snd_nxt;
        let rcv_nxt = s.rcv_nxt;
        let snd_wnd = s.snd_wnd;
        let rcv_wnd = s.rcv_wnd;
        let peer_mss = s.peer_mss;
        let rcv_wscale = s.our_wscale;
        let snd_wscale = s.snd_wscale;
        let wscale_enabled = s.wscale_enabled;
        let sack_permitted = s.sack_permitted;
        let ts_enabled = s.ts_enabled;
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, iss, iss);
                write_field!(slot, irs, irs);
                write_field!(slot, snd_una, snd_una);
                write_field!(slot, snd_nxt, snd_nxt);
                write_field!(slot, snd_wnd, snd_wnd);
                write_field!(slot, rcv_nxt, rcv_nxt);
                write_field!(slot, rcv_wnd, rcv_wnd);
                write_field!(slot, peer_mss, peer_mss);
                write_field!(slot, rcv_wscale, rcv_wscale);
                write_field!(slot, snd_wscale, snd_wscale);
                write_field!(slot, wscale_enabled, wscale_enabled);
                write_field!(slot, sack_permitted, sack_permitted);
                write_field!(slot, nagle_enabled, true);
                write_field!(slot, close_phase, ClosePhase::Established);
                write_field!(slot, rtt, RttEstimator::new());
                write_field!(slot, cc, CcAlgo::cubic(cc_mss));
                zero_field!(slot, sendmap);
                write_field!(slot, retransmit_token, None);
                write_field!(slot, keepalive_token, None);
                write_field!(slot, keepalive_probes_sent, 0u8);
                write_field!(slot, last_activity_tick, 0u64);
                write_field!(slot, fin_wait2_token, None);
                write_field!(slot, ts_enabled, ts_enabled);
                write_field!(slot, ts_recent, ts_recent);
                write_field!(slot, last_ack_sent, 0u32);
                write_field!(slot, reset_received, false);
                write_field!(slot, peer_closed, false);
                write_field!(
                    slot,
                    challenge_budget,
                    challenge_ack::ChallengeBudget::new()
                );
                Ok(slot.finish())
            },
        )
    }

    /// Test-only by-value constructor. Materialises the `Self` rvalue on the
    /// caller's stack, so only a consumer that immediately heap-moves it may
    /// call it; production code uses [`init_new`] / [`init_from_syn_recv`].
    #[cfg(any(test, feature = "test-hooks"))]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        iss: SeqNum,
        irs: SeqNum,
        snd_una: SeqNum,
        snd_nxt: SeqNum,
        rcv_nxt: SeqNum,
        snd_wnd: u32,
        rcv_wnd: u16,
        peer_mss: u16,
        snd_wscale: u8,
        rcv_wscale: u8,
        wscale_enabled: bool,
        ts_enabled: bool,
    ) -> Self {
        Self {
            iss,
            irs,
            snd_una,
            snd_nxt,
            snd_wnd,
            rcv_nxt,
            rcv_wnd,
            peer_mss,
            rcv_wscale,
            snd_wscale,
            wscale_enabled,
            sack_permitted: false,
            nagle_enabled: true,
            close_phase: ClosePhase::Established,
            rtt: RttEstimator::new(),
            cc: CcAlgo::cubic(peer_mss.max(DEFAULT_MSS) as u32),
            sendmap: SendMap::new(),
            retransmit_token: None,
            keepalive_token: None,
            keepalive_probes_sent: 0,
            last_activity_tick: 0,
            fin_wait2_token: None,
            ts_enabled,
            ts_recent: 0,
            last_ack_sent: 0,
            reset_received: false,
            peer_closed: false,
            challenge_budget: challenge_ack::ChallengeBudget::new(),
        }
    }

    /// Timestamp option for outgoing segments, `None` if not negotiated.
    #[inline]
    pub fn ts_option(&self, now_ms: u64) -> Option<(u32, u32)> {
        if self.ts_enabled {
            Some((now_ms as u32, self.ts_recent))
        } else {
            None
        }
    }

    /// Apply an incoming segment to a Data PCB.
    pub fn on_segment(
        pcb: &mut Pcb,
        bufs: &mut TcpBufferPair,
        hdr: &TcpHeader,
        options: &[u8],
        payload: &[u8],
        now_ms: u64,
    ) -> Actions {
        if hdr.is_rst() {
            return handle_data_rst(pcb, hdr, now_ms);
        }

        let mut actions = Actions::new();
        let tuple = pcb.tuple;

        // RFC 5961 §4: a SYN in a synchronized state is answered with a
        // challenge ACK irrespective of its sequence number, never a RST.
        if hdr.is_syn() {
            return Self::on_unexpected_syn(pcb, now_ms, actions);
        }

        // RFC 793 §3.9: everything below this point may only act on a segment
        // that falls in the receive window.
        if !data_segment_acceptable(pcb, hdr, payload.len()) {
            return unacceptable_segment_ack(pcb, now_ms, actions);
        }

        if data_paws_should_drop(pcb, options) {
            return paws_drop_ack(pcb, now_ms);
        }

        if !hdr.is_ack() {
            return actions;
        }

        // RFC 7323 §4.3: update ts_recent only when SEG.SEQ <= Last.ACK.sent.
        {
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            if data.ts_enabled {
                let parsed = super::super::header::parse_tcp_options(options);
                if let Some((tsval, _)) = parsed.timestamp {
                    if seq_le(hdr.seq_num, data.last_ack_sent) || data.last_ack_sent == 0 {
                        data.ts_recent = tsval;
                    }
                }
            }
        }

        let acked = {
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            data.process_ack(tuple, hdr, options, now_ms, &mut actions)
        };
        if acked > 0 {
            bufs.send.process_ack(acked as usize);
        }

        let transition =
            Self::process_payload_fin_and_ack(pcb, bufs, hdr, payload, now_ms, &mut actions);

        match transition {
            NextTransition::StayInData => {}
            NextTransition::ToTimeWait => {
                let PcbState::Data(data) = &pcb.state else {
                    unreachable!()
                };
                let tw = TimeWaitState::new(data.rcv_nxt, data.snd_nxt, data.rcv_wnd, now_ms);
                // Deferred through `Actions` so the glue layer installs the
                // 2×MSL timer after the lock drops; `key: 0` is a sentinel it
                // replaces with the real slot index.
                actions.push_timer(TimerOp::Schedule {
                    kind: TimerKind::TcpTimeWait,
                    key: 0,
                    delay_ms: TIME_WAIT_MS,
                });
                let _old = mem::replace(&mut pcb.state, PcbState::TimeWait(tw));
            }
            NextTransition::ReleaseNow => {
                actions.release = true;
            }
        }

        actions
    }

    fn on_rst(pcb: &mut Pcb, mut actions: Actions) -> Actions {
        let PcbState::Data(data) = &mut pcb.state else {
            unreachable!()
        };
        data.reset_received = true;
        if let Some(token) = data.retransmit_token.take() {
            actions.push_timer(TimerOp::Cancel { token });
        }
        if let Some(token) = data.keepalive_token.take() {
            actions.push_timer(TimerOp::Cancel { token });
        }
        actions.release = true;
        actions.notify |= SocketNotify::RESET_RECEIVED | SocketNotify::RECV_WAKE;
        actions
    }

    fn on_unexpected_syn(pcb: &mut Pcb, now_ms: u64, mut actions: Actions) -> Actions {
        let tuple = pcb.tuple;
        let PcbState::Data(data) = &mut pcb.state else {
            unreachable!()
        };
        let snd_nxt = data.snd_nxt.raw();
        let rcv_nxt = data.rcv_nxt.raw();
        let rcv_wnd = data.rcv_wnd;
        let ts_opt = data.ts_option(now_ms);
        if data.challenge_budget.try_consume(now_ms) {
            let mut ack = SegmentBuilder::ack(tuple, snd_nxt, rcv_nxt, rcv_wnd);
            ack.timestamp = ts_opt;
            actions.push_segment(ack);
        }
        actions
    }

    /// Returns the number of bytes newly acknowledged, 0 if the ACK did not
    /// advance `snd_una`.
    fn process_ack(
        &mut self,
        _tuple: super::super::tuple::TcpTuple,
        hdr: &TcpHeader,
        options: &[u8],
        now_ms: u64,
        actions: &mut Actions,
    ) -> u32 {
        let old_snd_una = self.snd_una;
        let ack = hdr.ack_num;

        let parsed = if (!options.is_empty() && self.sack_permitted) || self.ts_enabled {
            Some(super::super::header::parse_tcp_options(options))
        } else {
            None
        };

        let (sack_blocks, sack_count) = if self.sack_permitted {
            if let Some(ref p) = parsed {
                (p.sack_blocks, p.sack_block_count)
            } else {
                ([(0, 0); 4], 0)
            }
        } else {
            ([(0, 0); 4], 0)
        };

        // RFC 793 §3.4: an ACK is acceptable only in (snd_una, snd_nxt].
        let acked = if seq_gt(ack, old_snd_una.raw()) && seq_le(ack, self.snd_nxt.raw()) {
            self.snd_una = SeqNum::new(ack);
            self.snd_wnd = if self.wscale_enabled {
                (hdr.window_size as u32) << self.snd_wscale
            } else {
                hdr.window_size as u32
            };
            let acked = ack.wrapping_sub(old_snd_una.raw());
            let outcome = self.sendmap.on_cumulative_ack(self.snd_una);

            // RTT measurement: prefer RTTM (timestamps) over Karn.
            let rtt_sample = if self.ts_enabled {
                parsed
                    .as_ref()
                    .and_then(|p| p.timestamp)
                    .and_then(|(_, tsecr)| {
                        if tsecr != 0 {
                            Some((now_ms as u32).wrapping_sub(tsecr))
                        } else {
                            None
                        }
                    })
            } else {
                outcome
                    .rtt_sample_origin_ms
                    .map(|origin| now_ms.saturating_sub(origin) as u32)
            };
            if let Some(rtt_ms) = rtt_sample {
                self.rtt.sample(rtt_ms);
            }
            self.cc.on_ack(
                outcome.bytes_freed,
                rtt_sample,
                self.snd_una.raw(),
                self.snd_nxt.raw(),
                now_ms,
            );
            if let Some(token) = self.retransmit_token.take() {
                actions.push_timer(TimerOp::Cancel { token });
            }
            if !self.sendmap.is_empty() {
                let delay_ms = (self.rtt.rto_ms() as u64).max(1);
                actions.push_timer(TimerOp::Schedule {
                    kind: TimerKind::TcpRetransmit,
                    key: 0,
                    delay_ms,
                });
            }
            actions.notify |= SocketNotify::SEND_WAKE;
            acked
        } else {
            0
        };

        // SACK blocks feed RFC 6675 loss detection on duplicate ACKs too.
        if sack_count > 0 {
            let new_losses = self
                .sendmap
                .apply_sack_blocks(&sack_blocks[..sack_count as usize], sack_count);
            if new_losses && !self.cc.in_recovery() {
                self.cc
                    .on_fast_retransmit(self.sendmap.pipe(), self.snd_nxt.raw());
            }
        }

        acked
    }

    /// Payload, FIN and post-ACK transitions share mutable access to both
    /// `bufs` and `pcb.state`, so splitting them further would mean juggling
    /// borrows.
    fn process_payload_fin_and_ack(
        pcb: &mut Pcb,
        bufs: &mut TcpBufferPair,
        hdr: &TcpHeader,
        payload: &[u8],
        now_ms: u64,
        actions: &mut Actions,
    ) -> NextTransition {
        let tuple = pcb.tuple;

        let mut accepted_len: usize = 0;
        let PcbState::Data(d) = &pcb.state else {
            unreachable!()
        };
        let data_is_open = matches!(
            d.close_phase,
            ClosePhase::Established
                | ClosePhase::CloseWait
                | ClosePhase::FinWait1
                | ClosePhase::FinWait2
        );

        if !payload.is_empty() && data_is_open {
            let PcbState::Data(d) = &pcb.state else {
                unreachable!()
            };
            let expected_seq = d.rcv_nxt;
            if hdr.seq_num != expected_seq.raw() {
                // Buffer ahead of rcv_nxt and duplicate-ACK so the peer
                // retransmits the gap.
                if seq_gt(hdr.seq_num, expected_seq.raw()) {
                    let offset = hdr.seq_num.wrapping_sub(expected_seq.raw()) as usize;
                    let wrote_ooo = bufs.recv.buf.write_at_offset(offset, payload);
                    if wrote_ooo > 0 {
                        bufs.ooo.insert(hdr.seq_num, wrote_ooo);
                    }
                }
                let window = bufs.recv.window();
                let mut ack_seg =
                    SegmentBuilder::ack(tuple, d.snd_nxt.raw(), d.rcv_nxt.raw(), window);
                if d.sack_permitted {
                    let (ooo_blocks, ooo_count) = bufs.ooo.sack_blocks();
                    let seg_end = hdr.seq_num.wrapping_add(payload.len() as u32);

                    // DSACK (RFC 2883): a duplicate range goes in the first
                    // SACK block so the peer can detect spurious retransmits.
                    if super::super::seq::seq_lt(hdr.seq_num, expected_seq.raw()) {
                        let dsack_right = if seq_gt(seg_end, expected_seq.raw()) {
                            expected_seq.raw()
                        } else {
                            seg_end
                        };
                        ack_seg.sack_blocks[0] = (hdr.seq_num, dsack_right);
                        let mut total = 1u8;
                        for i in 0..ooo_count as usize {
                            if total >= 4 {
                                break;
                            }
                            ack_seg.sack_blocks[total as usize] = ooo_blocks[i];
                            total += 1;
                        }
                        ack_seg.sack_block_count = total;
                    } else {
                        ack_seg.sack_blocks = ooo_blocks;
                        ack_seg.sack_block_count = ooo_count;
                    }
                }
                ack_seg.timestamp = d.ts_option(now_ms);
                actions.push_segment(ack_seg);
                return NextTransition::StayInData;
            }
            let wrote = bufs.recv.enqueue(payload, now_ms);
            accepted_len = wrote;
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            data.rcv_nxt = data.rcv_nxt.wrapping_add(wrote as u32);
            if !bufs.ooo.is_empty() {
                let PcbState::Data(d) = &pcb.state else {
                    unreachable!()
                };
                let rcv_nxt = d.rcv_nxt;
                let drained = bufs.ooo.drain_contiguous(rcv_nxt.raw());
                if drained > 0 {
                    bufs.recv.buf.advance_head(drained);
                    bufs.recv.ack_pending = true;
                    bufs.recv.segments_since_ack = bufs.recv.segments_since_ack.saturating_add(1);
                    if bufs.recv.segments_since_ack == 1 {
                        bufs.recv.delayed_ack_deadline_ms =
                            now_ms.saturating_add(super::super::buffer::DELAYED_ACK_MS);
                    }
                    accepted_len += drained;
                    let PcbState::Data(data) = &mut pcb.state else {
                        unreachable!()
                    };
                    data.rcv_nxt = data.rcv_nxt.wrapping_add(drained as u32);
                }
            }
            let window = bufs.recv.window();
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            data.rcv_wnd = window;
            if accepted_len > 0 {
                actions.notify |= SocketNotify::RECV_WAKE;
            }
            if bufs.recv.should_ack_now(now_ms) {
                let PcbState::Data(data) = &mut pcb.state else {
                    unreachable!()
                };
                let mut ack = SegmentBuilder::ack(
                    tuple,
                    data.snd_nxt.raw(),
                    data.rcv_nxt.raw(),
                    data.rcv_wnd,
                );
                ack.timestamp = data.ts_option(now_ms);
                data.last_ack_sent = ack.ack_num;
                actions.push_segment(ack);
                bufs.recv.ack_sent();
                if !hdr.is_fin() {
                    return NextTransition::StayInData;
                }
            } else if !hdr.is_fin() {
                return NextTransition::StayInData;
            }
        }

        let ack = hdr.ack_num;
        {
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            match data.close_phase {
                ClosePhase::FinWait1 => {
                    if ack == data.snd_nxt.raw() {
                        if hdr.is_fin() {
                            // Simultaneous close: handled below.
                        } else {
                            data.close_phase = ClosePhase::FinWait2;
                            // The FIN_WAIT_2 timeout releases stale
                            // half-closed connections.
                            actions.push_timer(TimerOp::Schedule {
                                kind: TimerKind::TcpFinWait2,
                                key: 0,
                                delay_ms: super::super::FIN_WAIT2_TIMEOUT_MS,
                            });
                        }
                    }
                }
                ClosePhase::Closing => {
                    if ack == data.snd_nxt.raw() {
                        return NextTransition::ToTimeWait;
                    }
                }
                ClosePhase::LastAck => {
                    if ack == data.snd_nxt.raw() {
                        return NextTransition::ReleaseNow;
                    }
                }
                _ => {}
            }
        }

        if hdr.is_fin() {
            let tuple = pcb.tuple;
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            let fin_seq = hdr.seq_num.wrapping_add(accepted_len as u32);
            if fin_seq != data.rcv_nxt.raw() {
                let mut ack = SegmentBuilder::ack(
                    tuple,
                    data.snd_nxt.raw(),
                    data.rcv_nxt.raw(),
                    data.rcv_wnd,
                );
                ack.timestamp = data.ts_option(now_ms);
                actions.push_segment(ack);
                return NextTransition::StayInData;
            }
            data.rcv_nxt = data.rcv_nxt.wrapping_add(1);
            data.peer_closed = true;
            let new_phase = match data.close_phase {
                ClosePhase::Established => ClosePhase::CloseWait,
                ClosePhase::FinWait1 => {
                    if hdr.ack_num == data.snd_nxt.raw() {
                        data.close_phase = ClosePhase::Closing;
                        let mut ack = SegmentBuilder::ack(
                            tuple,
                            data.snd_nxt.raw(),
                            data.rcv_nxt.raw(),
                            data.rcv_wnd,
                        );
                        ack.timestamp = data.ts_option(now_ms);
                        actions.push_segment(ack);
                        return NextTransition::ToTimeWait;
                    }
                    ClosePhase::Closing
                }
                ClosePhase::FinWait2 => {
                    if let Some(token) = data.fin_wait2_token.take() {
                        actions.push_timer(TimerOp::Cancel { token });
                    }
                    data.close_phase = ClosePhase::Closing;
                    let mut ack = SegmentBuilder::ack(
                        tuple,
                        data.snd_nxt.raw(),
                        data.rcv_nxt.raw(),
                        data.rcv_wnd,
                    );
                    ack.timestamp = data.ts_option(now_ms);
                    actions.push_segment(ack);
                    return NextTransition::ToTimeWait;
                }
                other => other,
            };
            data.close_phase = new_phase;
            actions.notify |= SocketNotify::PEER_CLOSED | SocketNotify::RECV_WAKE;
            let mut ack =
                SegmentBuilder::ack(tuple, data.snd_nxt.raw(), data.rcv_nxt.raw(), data.rcv_wnd);
            ack.timestamp = data.ts_option(now_ms);
            actions.push_segment(ack);
        }

        NextTransition::StayInData
    }

    /// The idle delay in milliseconds the caller should schedule, or `None` if
    /// keepalive is disabled or a timer is already active.
    pub fn schedule_initial_keepalive(&mut self, keepalive_enabled: bool) -> Option<u64> {
        if keepalive_enabled && self.keepalive_token.is_none() {
            Some(super::super::TCP_KEEPALIVE_IDLE_MS)
        } else {
            None
        }
    }

    /// The old token to cancel and the idle delay in milliseconds to
    /// reschedule, or `None` if keepalive was not active.
    pub fn reset_keepalive_on_activity(&mut self) -> Option<(TimerToken, u64)> {
        if let Some(token) = self.keepalive_token.take() {
            self.keepalive_probes_sent = 0;
            Some((token, super::super::TCP_KEEPALIVE_IDLE_MS))
        } else {
            None
        }
    }

    /// The ACK segment if a delayed ACK is now due, marking it sent on the
    /// receive buffer.
    pub fn check_delayed_ack(
        &self,
        tuple: super::super::tuple::TcpTuple,
        bufs: &mut TcpBufferPair,
        now_ms: u64,
    ) -> Option<super::super::segment::TcpOutSegment> {
        if bufs.recv.should_ack_now(now_ms) {
            let window = bufs.recv.window();
            let mut seg =
                SegmentBuilder::ack(tuple, self.snd_nxt.raw(), self.rcv_nxt.raw(), window);
            seg.timestamp = self.ts_option(now_ms);
            bufs.recv.ack_sent();
            Some(seg)
        } else {
            None
        }
    }

    /// Generate a zero-window probe if `snd_wnd == 0` and there is
    /// buffered data to send.
    pub fn check_zero_window_probe(
        &self,
        tuple: super::super::tuple::TcpTuple,
        bufs: &super::super::buffer::TcpBufferPair,
    ) -> Option<super::super::segment::TcpOutSegment> {
        if self.snd_wnd != 0 || bufs.send.buffered_len() == 0 {
            return None;
        }
        let mut byte = [0u8; 1];
        if bufs.send.peek_unsent(&mut byte) == 0 {
            return None;
        }
        let window = bufs.recv.window();
        let mut seg =
            SegmentBuilder::data_push(tuple, self.snd_nxt.raw(), self.rcv_nxt.raw(), window);
        seg.timestamp = self.ts_option(super::super::clock::now_ms());
        Some(seg)
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_assert_invariants(&self, _pcb: &Pcb) {
        debug_assert!(
            self.snd_una <= self.snd_nxt,
            "Data: snd_una ({}) > snd_nxt ({})",
            self.snd_una.raw(),
            self.snd_nxt.raw()
        );
        match self.close_phase {
            ClosePhase::Established => {}
            ClosePhase::FinWait1 | ClosePhase::FinWait2 => {
                debug_assert!(self.snd_nxt >= self.snd_una, "FinWait: snd_nxt >= snd_una");
            }
            ClosePhase::CloseWait => {
                debug_assert!(self.peer_closed, "CloseWait implies peer_closed");
            }
            ClosePhase::Closing | ClosePhase::LastAck => {
                debug_assert!(self.peer_closed, "Closing/LastAck implies peer_closed");
            }
        }
        // FIN consumes one sequence byte but the send map covers data
        // segments only.
        let fin_offset = match self.close_phase {
            ClosePhase::FinWait1 | ClosePhase::LastAck | ClosePhase::Closing => 1u32,
            _ => 0,
        };
        // Our SYN occupies `iss` and is likewise not in the send map. It is
        // still outstanding whenever `snd_una` has not moved past it, which is
        // what a close out of `SYN_RECEIVED` leaves behind.
        let syn_offset = if self.snd_una.raw() == self.iss.raw() {
            1u32
        } else {
            0
        };
        let expected = self
            .snd_una
            .distance_to(self.snd_nxt)
            .saturating_sub(fin_offset)
            .saturating_sub(syn_offset);
        debug_assert_eq!(
            self.sendmap.total_bytes(),
            expected,
            "sendmap total_bytes ({}) != snd_nxt - snd_una - fin_offset ({})",
            self.sendmap.total_bytes(),
            expected,
        );
    }
}

/// Handle an incoming RST on a Data PCB (RFC 5961 classification).
///
/// `#[inline(never)]` here and on the PAWS helpers below keeps their
/// `SegmentBuilder` scratch and 400 B `Actions` slots out of the `on_segment`
/// dispatcher's frame, which the stack-safety gate bounds.
#[inline(never)]
fn handle_data_rst(pcb: &mut Pcb, hdr: &TcpHeader, now_ms: u64) -> Actions {
    let tuple = pcb.tuple;
    let mut actions = Actions::new();
    let PcbState::Data(data) = &pcb.state else {
        unreachable!()
    };
    let effective_wnd = if data.wscale_enabled {
        (data.rcv_wnd as u32) << data.rcv_wscale
    } else {
        data.rcv_wnd as u32
    };
    let rcv_nxt = data.rcv_nxt.raw();
    let snd_nxt = data.snd_nxt.raw();
    let rcv_wnd = data.rcv_wnd;
    let ts_opt = data.ts_option(now_ms);
    match challenge_ack::classify_rst(hdr.seq_num, rcv_nxt, effective_wnd) {
        challenge_ack::RstAction::Accept => DataState::on_rst(pcb, actions),
        challenge_ack::RstAction::ChallengeAck => {
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            if data.challenge_budget.try_consume(now_ms) {
                let mut ack = SegmentBuilder::ack(tuple, snd_nxt, rcv_nxt, rcv_wnd);
                ack.timestamp = ts_opt;
                actions.push_segment(ack);
            }
            actions
        }
        challenge_ack::RstAction::Drop => actions,
    }
}

/// RFC 793 §3.9 acceptability for a Data-state segment, with the receive
/// window scaled as negotiated.
#[inline(never)]
fn data_segment_acceptable(pcb: &Pcb, hdr: &TcpHeader, payload_len: usize) -> bool {
    let PcbState::Data(data) = &pcb.state else {
        return false;
    };
    let effective_wnd = if data.wscale_enabled {
        (data.rcv_wnd as u32) << data.rcv_wscale
    } else {
        data.rcv_wnd as u32
    };
    let seg_len = payload_len as u32 + u32::from(hdr.is_fin());
    challenge_ack::segment_acceptable(hdr.seq_num, seg_len, data.rcv_nxt.raw(), effective_wnd)
}

/// RFC 793 §3.9: an unacceptable segment is dropped, and unless it carried a
/// RST an ACK is sent back so the peer resynchronises.
#[inline(never)]
fn unacceptable_segment_ack(pcb: &Pcb, now_ms: u64, mut actions: Actions) -> Actions {
    let PcbState::Data(data) = &pcb.state else {
        return actions;
    };
    let mut ack = SegmentBuilder::ack(
        pcb.tuple,
        data.snd_nxt.raw(),
        data.rcv_nxt.raw(),
        data.rcv_wnd,
    );
    ack.timestamp = data.ts_option(now_ms);
    actions.push_segment(ack);
    actions
}

/// Does the incoming segment's timestamp option trip PAWS?
#[inline(never)]
fn data_paws_should_drop(pcb: &Pcb, options: &[u8]) -> bool {
    let PcbState::Data(data) = &pcb.state else {
        return false;
    };
    if !data.ts_enabled || data.ts_recent == 0 {
        return false;
    }
    let parsed = super::super::header::parse_tcp_options(options);
    let Some((tsval, _)) = parsed.timestamp else {
        return false;
    };
    super::super::header::ts_less_than(tsval, data.ts_recent)
}

/// Build the drop-ACK [`Actions`] for a PAWS-rejected segment.
#[inline(never)]
fn paws_drop_ack(pcb: &Pcb, now_ms: u64) -> Actions {
    let mut actions = Actions::new();
    let PcbState::Data(data) = &pcb.state else {
        return actions;
    };
    let mut ack = SegmentBuilder::ack(
        pcb.tuple,
        data.snd_nxt.raw(),
        data.rcv_nxt.raw(),
        data.rcv_wnd,
    );
    ack.timestamp = data.ts_option(now_ms);
    actions.push_segment(ack);
    actions
}
