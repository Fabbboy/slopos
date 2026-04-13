//! `Data` state: active data transfer and the closing-phase chain.
//!
//! Covers every RFC 793 substate that shares a send window, receive
//! window, RTT estimator, and congestion controller — that is,
//! `ESTABLISHED`, `FIN_WAIT_1`, `FIN_WAIT_2`, `CLOSE_WAIT`, `CLOSING`,
//! and `LAST_ACK`.  Which of those six labels currently applies is
//! captured by the [`ClosePhase`] sub-enum on [`DataState`].
//!
//! # Handler structure
//!
//! [`DataState::on_segment`] is the single input entry point.  It
//! routes the work to five sub-methods, each responsible for one
//! phase of the RFC 793 §3.4 arrival procedure:
//!
//! 1. [`on_rst`] — handle RST fast-path (release or mark reset).
//! 2. [`on_unexpected_syn`] — handle SYN-in-established (RST + release).
//! 3. [`process_ack`] — update `snd_una`/`snd_wnd`, drive
//!    [`RetxQueue::on_ack`], [`RttEstimator::sample`], and
//!    [`CongestionControl::on_ack`].  Reschedules the RTO timer.
//! 4. [`process_payload`] — accept in-order bytes into the recv
//!    buffer, buffer OOO segments, drain contiguous ones back out.
//! 5. [`process_fin_and_close_phase`] — advance [`ClosePhase`] in
//!    response to the peer's FIN flag; signal a `ToTimeWait`
//!    transition to the caller when the closing chain ends.
//! 6. [`emit_ack_if_needed`] — delayed-ACK decision.

use core::mem;

use super::super::actions::{Actions, SocketNotify, TimerOp};
use super::super::buffer::TcpBufferPair;
use super::super::cong::{CcAlgo, CongestionControl};
use super::super::header::{DEFAULT_MSS, TcpHeader};
use super::super::retx::RetxQueue;
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

/// Internal transition hint — the sub-methods signal to the top
/// dispatcher when a change of variant (DataState → TimeWait) or a
/// slot release is required.  Allows each sub-method to keep
/// operating on `&mut DataState` without owning the enum swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NextTransition {
    StayInData,
    ToTimeWait,
    ReleaseNow,
}

/// State-specific payload for the Data variant.
#[derive(Debug)]
pub struct DataState {
    // -------- Sequence variables (RFC 793 §3.2) --------
    pub iss: SeqNum,
    pub irs: SeqNum,
    pub snd_una: SeqNum,
    pub snd_nxt: SeqNum,
    pub snd_wnd: u32,
    pub rcv_nxt: SeqNum,
    pub rcv_wnd: u16,

    // -------- Options --------
    pub peer_mss: u16,
    pub rcv_wscale: u8,
    pub snd_wscale: u8,
    pub wscale_enabled: bool,
    pub sack_permitted: bool,
    pub nagle_enabled: bool,

    // -------- Close phase --------
    pub close_phase: ClosePhase,

    // -------- RTT + congestion control --------
    pub rtt: RttEstimator,
    pub cc: CcAlgo,

    // -------- Retransmission --------
    pub retx: RetxQueue,
    pub retransmit_token: Option<TimerToken>,
    pub dup_ack_count: u8,

    // -------- Keepalive --------
    pub keepalive_token: Option<TimerToken>,
    pub keepalive_probes_sent: u8,
    pub last_activity_tick: u64,

    // -------- SACK scoreboard (RFC 2018) --------
    /// SACK blocks last received from the peer (left_edge, right_edge).
    pub sack_scoreboard: [(u32, u32); 4],
    pub sack_scoreboard_count: u8,

    // -------- Misc --------
    pub reset_received: bool,
    pub peer_closed: bool,
}

impl DataState {
    /// Create a fresh `DataState` for a newly-established connection.
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
            cc: CcAlgo::new_reno(peer_mss.max(DEFAULT_MSS) as u32),
            retx: RetxQueue::new(),
            retransmit_token: None,
            dup_ack_count: 0,
            keepalive_token: None,
            keepalive_probes_sent: 0,
            last_activity_tick: 0,
            sack_scoreboard: [(0, 0); 4],
            sack_scoreboard_count: 0,
            reset_received: false,
            peer_closed: false,
        }
    }

    /// Create a `DataState` from a `SynRecvState` that has completed
    /// the 3-way handshake.  Used by the lifecycle close path when
    /// closing from `SYN_RECEIVED`.
    pub fn from_syn_recv(s: &super::syn_recv::SynRecvState) -> Self {
        let mut ds = Self::new(
            s.iss,
            s.irs,
            s.snd_una,
            s.snd_nxt,
            s.rcv_nxt,
            s.snd_wnd,
            s.rcv_wnd,
            s.peer_mss,
            s.snd_wscale,
            s.our_wscale,
            s.wscale_enabled,
        );
        ds.sack_permitted = s.sack_permitted;
        ds
    }

    /// Apply an incoming segment to a Data PCB.  See module doc for
    /// the sub-method breakdown.
    pub fn on_segment(
        pcb: &mut Pcb,
        bufs: &mut TcpBufferPair,
        hdr: &TcpHeader,
        options: &[u8],
        payload: &[u8],
        now_ms: u64,
    ) -> Actions {
        let mut actions = Actions::new();
        let tuple = pcb.tuple;

        // Grab a mutable view of the Data payload.
        let PcbState::Data(_) = &pcb.state else {
            unreachable!("DataState::on_segment called with non-Data state");
        };

        // Fast-paths for RST and unexpected SYN.
        if hdr.is_rst() {
            return Self::on_rst(pcb, actions);
        }
        if hdr.is_syn() {
            return Self::on_unexpected_syn(pcb, hdr, actions);
        }
        // Everything past this point requires an ACK flag.
        if !hdr.is_ack() {
            return actions;
        }

        // Process in the canonical RFC 793 order.  The sub-methods
        // signal transitions via the returned `NextTransition`.
        // Process ACK and free acknowledged bytes from the send buffer.
        let acked = {
            let Some(data) = Self::as_mut(&mut pcb.state) else {
                unreachable!()
            };
            data.process_ack(tuple, hdr, options, now_ms, &mut actions)
        };
        if acked > 0 {
            bufs.send.process_ack(acked as usize);
        }

        // -------- Fast retransmit (RFC 5681 §3.2) --------
        // Trigger on the exact 3rd duplicate ACK, only if not already
        // in recovery.  Rewinds the send cursor so the next poll_transmit
        // re-sends from snd_una with a halved congestion window.
        {
            let should_fast_retransmit = {
                let Some(data) = Self::as_ref(&pcb.state) else {
                    unreachable!()
                };
                data.dup_ack_count == 3 && !data.cc.in_recovery()
            };
            if should_fast_retransmit {
                let Some(data) = Self::as_mut(&mut pcb.state) else {
                    unreachable!()
                };
                let flight = data.retx.inflight_bytes();
                data.cc.on_fast_retransmit(flight, data.snd_nxt.raw());
                data.retx.clear();
                data.snd_nxt = data.snd_una;
                bufs.send.inflight = 0;
                bufs.send.needs_retransmit = true;
            }
        }

        let next = next_stub_placeholder();

        // `process_payload` + `process_fin_and_close_phase` need
        // combined access to bufs and pcb.state; do them
        // with a helper that takes both.
        let transition =
            Self::process_payload_fin_and_ack(pcb, bufs, hdr, payload, now_ms, &mut actions, next);

        // Apply any variant transition signalled by the sub-methods.
        match transition {
            NextTransition::StayInData => {}
            NextTransition::ToTimeWait => {
                // Move the final rcv_nxt / snd_nxt / rcv_wnd out of
                // the Data payload into a new TimeWaitState.
                let Some(data) = Self::as_ref(&pcb.state) else {
                    unreachable!()
                };
                let tw = TimeWaitState::new(data.rcv_nxt, data.snd_nxt, data.rcv_wnd, now_ms);
                // Schedule the 2×MSL timer via Actions so the glue
                // layer can install it after the lock is released.
                // The slot index is filled by the glue layer; we use
                // a sentinel (0) and the glue substitutes the real
                // value.  See `tcp::input` in the C.1 cutover.
                let delay_ticks = ((TIME_WAIT_MS as u64) / 10).max(1);
                actions.push_timer(TimerOp::Schedule {
                    kind: TimerKind::TcpTimeWait,
                    key: 0,
                    delay_ticks,
                });
                let _old = mem::replace(&mut pcb.state, PcbState::TimeWait(tw));
            }
            NextTransition::ReleaseNow => {
                actions.release = true;
            }
        }

        actions
    }

    // -------------------------------------------------------------------------
    // Sub-methods
    // -------------------------------------------------------------------------

    /// Handle an incoming RST.  The connection is dead; flag the
    /// reset to the socket layer and release the slot.
    fn on_rst(pcb: &mut Pcb, mut actions: Actions) -> Actions {
        let Some(data) = Self::as_mut(&mut pcb.state) else {
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

    /// Handle an unexpected SYN in an established+ state: send RST,
    /// release the slot, flag the reset.
    fn on_unexpected_syn(pcb: &mut Pcb, _hdr: &TcpHeader, mut actions: Actions) -> Actions {
        let tuple = pcb.tuple;
        let Some(data) = Self::as_ref(&pcb.state) else {
            unreachable!()
        };
        actions.push_segment(SegmentBuilder::bare_rst(tuple, data.snd_nxt.raw()));
        actions.release = true;
        actions.notify |= SocketNotify::RESET_RECEIVED | SocketNotify::RECV_WAKE;
        actions
    }

    /// Advance `snd_una`/`snd_wnd`, pop newly-acked entries from the
    /// retx queue, drive RTT / congestion-control callbacks, and
    /// reschedule the retransmission timer.  Returns the number of
    /// bytes newly acknowledged (0 if the ACK did not advance).
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

        // Parse SACK blocks from the peer's ACK (RFC 2018).
        if self.sack_permitted && !options.is_empty() {
            let parsed = super::super::header::parse_tcp_options(options);
            if parsed.sack_block_count > 0 {
                self.sack_scoreboard = parsed.sack_blocks;
                self.sack_scoreboard_count = parsed.sack_block_count;
            }
        }

        // Only advance if the ACK is strictly greater than snd_una
        // and no greater than snd_nxt (RFC 793 §3.4).
        if seq_gt(ack, old_snd_una.raw()) && seq_le(ack, self.snd_nxt.raw()) {
            self.snd_una = SeqNum::new(ack);
            self.snd_wnd = if self.wscale_enabled {
                (hdr.window_size as u32) << self.snd_wscale
            } else {
                hdr.window_size as u32
            };
            let acked = ack.wrapping_sub(old_snd_una.raw());
            // Pop retx entries; the oldest freed non-retransmitted
            // entry gives us an RTT sample (Karn).
            let outcome = self.retx.on_ack(self.snd_una);
            if let Some(origin_ms) = outcome.rtt_sample_origin_ms {
                let rtt_ms = now_ms.saturating_sub(origin_ms) as u32;
                self.rtt.sample(rtt_ms);
            }
            // Feed CC with the freshly-acked bytes + any RTT sample.
            self.cc.on_ack(
                outcome.bytes_freed,
                outcome
                    .rtt_sample_origin_ms
                    .map(|origin| now_ms.saturating_sub(origin) as u32),
            );
            self.dup_ack_count = 0;
            // Clear SACK scoreboard — forward ACK supersedes old blocks.
            self.sack_scoreboard_count = 0;
            // Reschedule the retransmit timer.
            if let Some(token) = self.retransmit_token.take() {
                actions.push_timer(TimerOp::Cancel { token });
            }
            if !self.retx.is_empty() {
                let delay_ticks = (self.rtt.rto_ms() as u64 / 10).max(1);
                actions.push_timer(TimerOp::Schedule {
                    kind: TimerKind::TcpRetransmit,
                    key: 0,
                    delay_ticks,
                });
            }
            actions.notify |= SocketNotify::SEND_WAKE;
            acked
        } else if ack == old_snd_una.raw() && self.snd_nxt > old_snd_una {
            // Duplicate ACK (same ack_num, still have inflight data).
            self.cc.on_dup_ack();
            self.dup_ack_count = self.dup_ack_count.saturating_add(1);
            0
        } else {
            0
        }
    }

    // -------------------------------------------------------------------------
    // Combined payload + FIN + delayed ACK handling
    // -------------------------------------------------------------------------

    /// Process payload, FIN, and any post-ACK state transitions in one
    /// place — they share mutable access to both `bufs` and
    /// `pcb.state`, so splitting further would require juggling
    /// mutable borrows.
    fn process_payload_fin_and_ack(
        pcb: &mut Pcb,
        bufs: &mut TcpBufferPair,
        hdr: &TcpHeader,
        payload: &[u8],
        now_ms: u64,
        actions: &mut Actions,
        _acked_hint: NextTransition,
    ) -> NextTransition {
        let tuple = pcb.tuple;

        // -------- Payload accept + OOO drain --------
        let mut accepted_len: usize = 0;
        let data_is_open = matches!(
            Self::as_ref(&pcb.state).map(|d| d.close_phase),
            Some(
                ClosePhase::Established
                    | ClosePhase::CloseWait
                    | ClosePhase::FinWait1
                    | ClosePhase::FinWait2
            )
        );

        if !payload.is_empty() && data_is_open {
            let expected_seq = Self::as_ref(&pcb.state).unwrap().rcv_nxt;
            if hdr.seq_num != expected_seq.raw() {
                // Out-of-order — buffer ahead of rcv_nxt and emit
                // a duplicate ACK so the peer retransmits the gap.
                if seq_gt(hdr.seq_num, expected_seq.raw()) {
                    bufs.ooo.insert(hdr.seq_num, payload);
                }
                let data = Self::as_ref(&pcb.state).unwrap();
                let window = bufs.recv.window();
                let mut ack_seg =
                    SegmentBuilder::ack(tuple, data.snd_nxt.raw(), data.rcv_nxt.raw(), window);
                // Include SACK blocks so the peer knows which OOO
                // ranges we hold (RFC 2018).
                if data.sack_permitted {
                    let (blocks, count) = bufs.ooo.sack_blocks();
                    ack_seg.sack_blocks = blocks;
                    ack_seg.sack_block_count = count;
                }
                actions.push_segment(ack_seg);
                return NextTransition::StayInData;
            }
            let wrote = bufs.recv.enqueue(payload, now_ms);
            accepted_len = wrote;
            if let Some(data) = Self::as_mut(&mut pcb.state) {
                data.rcv_nxt = data.rcv_nxt.wrapping_add(wrote as u32);
            }
            if !bufs.ooo.is_empty() {
                let rcv_nxt = Self::as_ref(&pcb.state).unwrap().rcv_nxt;
                let drained = bufs
                    .ooo
                    .drain_contiguous(rcv_nxt.raw(), &mut bufs.recv, now_ms);
                if drained > 0 {
                    accepted_len += drained;
                    if let Some(data) = Self::as_mut(&mut pcb.state) {
                        data.rcv_nxt = data.rcv_nxt.wrapping_add(drained as u32);
                    }
                }
            }
            let window = bufs.recv.window();
            if let Some(data) = Self::as_mut(&mut pcb.state) {
                data.rcv_wnd = window;
            }
            if accepted_len > 0 {
                actions.notify |= SocketNotify::RECV_WAKE;
            }
            // Emit an immediate ACK if delayed-ACK heuristic says so.
            if bufs.recv.should_ack_now(now_ms) {
                let data = Self::as_ref(&pcb.state).unwrap();
                actions.push_segment(SegmentBuilder::ack(
                    tuple,
                    data.snd_nxt.raw(),
                    data.rcv_nxt.raw(),
                    data.rcv_wnd,
                ));
                bufs.recv.ack_sent();
                if !hdr.is_fin() {
                    return NextTransition::StayInData;
                }
            } else if !hdr.is_fin() {
                return NextTransition::StayInData;
            }
        }

        // -------- State-specific pure-ACK transitions --------
        // These occur when an ACK moves FinWait1 → FinWait2,
        // Closing → TimeWait, or LastAck → released.
        let ack = hdr.ack_num;
        {
            let Some(data) = Self::as_mut(&mut pcb.state) else {
                unreachable!()
            };
            match data.close_phase {
                ClosePhase::FinWait1 => {
                    if ack == data.snd_nxt.raw() {
                        if hdr.is_fin() {
                            // Simultaneous close: handled below.
                        } else {
                            data.close_phase = ClosePhase::FinWait2;
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

        // -------- FIN handling --------
        if hdr.is_fin() {
            let tuple = pcb.tuple;
            let Some(data) = Self::as_mut(&mut pcb.state) else {
                unreachable!()
            };
            let fin_seq = hdr.seq_num.wrapping_add(accepted_len as u32);
            if fin_seq != data.rcv_nxt.raw() {
                // FIN arrived ahead of some payload — ignore the FIN
                // and just re-ACK the current cursor.
                actions.push_segment(SegmentBuilder::ack(
                    tuple,
                    data.snd_nxt.raw(),
                    data.rcv_nxt.raw(),
                    data.rcv_wnd,
                ));
                return NextTransition::StayInData;
            }
            data.rcv_nxt = data.rcv_nxt.wrapping_add(1);
            data.peer_closed = true;
            let new_phase = match data.close_phase {
                ClosePhase::Established => ClosePhase::CloseWait,
                ClosePhase::FinWait1 => {
                    // Our FIN not yet acked + peer FIN → Closing,
                    // unless this same segment also carries the ACK
                    // for our FIN (simultaneous close → TimeWait).
                    if hdr.ack_num == data.snd_nxt.raw() {
                        // Simultaneous close confirmed.
                        data.close_phase = ClosePhase::Closing; // transient
                        // Emit an ACK for the peer's FIN + signal
                        // the transition out of Data.
                        actions.push_segment(SegmentBuilder::ack(
                            tuple,
                            data.snd_nxt.raw(),
                            data.rcv_nxt.raw(),
                            data.rcv_wnd,
                        ));
                        return NextTransition::ToTimeWait;
                    }
                    ClosePhase::Closing
                }
                ClosePhase::FinWait2 => {
                    data.close_phase = ClosePhase::Closing;
                    actions.push_segment(SegmentBuilder::ack(
                        tuple,
                        data.snd_nxt.raw(),
                        data.rcv_nxt.raw(),
                        data.rcv_wnd,
                    ));
                    return NextTransition::ToTimeWait;
                }
                other => other,
            };
            data.close_phase = new_phase;
            actions.notify |= SocketNotify::PEER_CLOSED | SocketNotify::RECV_WAKE;
            // Emit an ACK for the FIN.
            actions.push_segment(SegmentBuilder::ack(
                tuple,
                data.snd_nxt.raw(),
                data.rcv_nxt.raw(),
                data.rcv_wnd,
            ));
        }

        NextTransition::StayInData
    }

    // -------------------------------------------------------------------------
    // Helpers for pattern-matching the Data variant
    // -------------------------------------------------------------------------

    #[inline]
    fn as_ref(state: &PcbState) -> Option<&DataState> {
        if let PcbState::Data(d) = state {
            Some(d)
        } else {
            None
        }
    }

    #[inline]
    fn as_mut(state: &mut PcbState) -> Option<&mut DataState> {
        if let PcbState::Data(d) = state {
            Some(d)
        } else {
            None
        }
    }

    // -------------------------------------------------------------------------
    // Keepalive / delayed-ACK / zero-window (D.5 extracted methods)
    // -------------------------------------------------------------------------

    /// If keepalive is enabled and no timer is active, return the idle
    /// tick count to schedule.  Caller is responsible for calling
    /// `NET_TIMER_WHEEL.schedule()` with this value.
    pub fn schedule_initial_keepalive(&mut self, keepalive_enabled: bool) -> Option<u64> {
        if keepalive_enabled && self.keepalive_token.is_none() {
            Some(super::super::TCP_KEEPALIVE_IDLE_TICKS)
        } else {
            None
        }
    }

    /// Reset the keepalive timer on data activity.  Returns the old
    /// token to cancel and the idle tick count to reschedule, or `None`
    /// if keepalive was not active.
    pub fn reset_keepalive_on_activity(&mut self) -> Option<(TimerToken, u64)> {
        if let Some(token) = self.keepalive_token.take() {
            self.keepalive_probes_sent = 0;
            Some((token, super::super::TCP_KEEPALIVE_IDLE_TICKS))
        } else {
            None
        }
    }

    /// Check if a delayed ACK should fire.  Returns the ACK segment
    /// if so, and marks the ACK as sent on the receive buffer.
    pub fn check_delayed_ack(
        &self,
        tuple: super::super::tuple::TcpTuple,
        bufs: &mut TcpBufferPair,
        now_ms: u64,
    ) -> Option<super::super::segment::TcpOutSegment> {
        if bufs.recv.should_ack_now(now_ms) {
            let window = bufs.recv.window();
            let seg = SegmentBuilder::ack(tuple, self.snd_nxt.raw(), self.rcv_nxt.raw(), window);
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
        Some(SegmentBuilder::data_push(
            tuple,
            self.snd_nxt.raw(),
            self.rcv_nxt.raw(),
            window,
        ))
    }

    // -------------------------------------------------------------------------
    // Invariants
    // -------------------------------------------------------------------------

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
        // FIN consumes 1 sequence byte but is not tracked in retx
        // (which only covers data segments).
        let fin_offset = match self.close_phase {
            ClosePhase::FinWait1 | ClosePhase::LastAck | ClosePhase::Closing => 1u32,
            _ => 0,
        };
        let expected = self
            .snd_una
            .distance_to(self.snd_nxt)
            .saturating_sub(fin_offset);
        debug_assert_eq!(
            self.retx.inflight_bytes(),
            expected,
            "retx inflight ({}) != snd_nxt - snd_una - fin_offset ({})",
            self.retx.inflight_bytes(),
            expected,
        );
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Placeholder returned from the ACK sub-method before the combined
/// payload/FIN block consumes the hint.  Always `StayInData` today.
fn next_stub_placeholder() -> NextTransition {
    NextTransition::StayInData
}
