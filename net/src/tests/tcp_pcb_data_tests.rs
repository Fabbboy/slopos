//! Unit tests for `tcp::pcb::data::DataState::on_segment`.
//!
//! Covers the six RFC 793 closing substates via the `ClosePhase`
//! sub-enum plus the algorithmic wire-in of RTT, congestion control,
//! and the retransmit queue.  Each test constructs a `Pcb` in a
//! specific `ClosePhase` directly, drives a synthetic segment, and
//! asserts on the resulting `Actions` + mutated state.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::tcp::actions::SocketNotify;
use crate::tcp::buffer::TcpBufferPair;
use crate::tcp::header::{
    TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_PSH, TCP_FLAG_RST, TCP_FLAG_SYN, TcpHeader,
};
use crate::tcp::pcb::data::{ClosePhase, DataState};
use crate::tcp::pcb::{Pcb, PcbState};
use crate::tcp::seq::SeqNum;
use crate::tcp::tuple::TcpTuple;

const LOCAL_IP: [u8; 4] = [10, 0, 0, 1];
const REMOTE_IP: [u8; 4] = [10, 0, 0, 2];
const LOCAL_PORT: u16 = 49_152;
const REMOTE_PORT: u16 = 80;
const OUR_ISS: u32 = 10_000;
const PEER_IRS: u32 = 20_000;

fn make_pcb_in_phase(phase: ClosePhase) -> Pcb {
    let tuple = TcpTuple {
        local_ip: LOCAL_IP,
        local_port: LOCAL_PORT,
        remote_ip: REMOTE_IP,
        remote_port: REMOTE_PORT,
    };
    let mut data = DataState::new(
        SeqNum::new(OUR_ISS),
        SeqNum::new(PEER_IRS),
        SeqNum::new(OUR_ISS + 1),  // snd_una after handshake
        SeqNum::new(OUR_ISS + 1),  // snd_nxt initially at snd_una
        SeqNum::new(PEER_IRS + 1), // rcv_nxt = irs + 1
        65_535,                    // snd_wnd
        32_768,                    // rcv_wnd
        1460,                      // peer_mss
        0,                         // snd_wscale
        0,                         // rcv_wscale
        false,                     // wscale_enabled
        false,                     // ts_enabled
    );
    data.close_phase = phase;
    if matches!(
        phase,
        ClosePhase::CloseWait | ClosePhase::Closing | ClosePhase::LastAck
    ) {
        data.peer_closed = true;
    }
    Pcb::new(tuple, PcbState::Data(data))
}

fn hdr(flags: u8, seq: u32, ack: u32) -> TcpHeader {
    TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: LOCAL_PORT,
        seq_num: seq,
        ack_num: ack,
        data_offset: 5,
        flags,
        window_size: 32_768,
        checksum: 0,
        urgent_ptr: 0,
    }
}

fn data_ref(pcb: &Pcb) -> &DataState {
    match &pcb.state {
        PcbState::Data(d) => d,
        _ => panic!("expected Data state"),
    }
}

// =============================================================================
// RST fast path
// =============================================================================

pub fn test_data_rst_releases_and_notifies() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    // RFC 5961: RST must have seq == rcv_nxt to be accepted.
    let rcv_nxt = PEER_IRS + 1;
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_RST, rcv_nxt, 0),
        &[],
        &[],
        0,
    );
    assert_test!(actions.release, "release flag set");
    assert_test!(
        actions.notify.contains(SocketNotify::RESET_RECEIVED),
        "RESET_RECEIVED"
    );
    assert_test!(
        actions.notify.contains(SocketNotify::RECV_WAKE),
        "RECV_WAKE"
    );
    pass!()
}

// =============================================================================
// Unexpected SYN → RST + release
// =============================================================================

pub fn test_data_unexpected_syn_triggers_rst_and_release() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    let actions = DataState::on_segment(&mut pcb, &mut bufs, &hdr(TCP_FLAG_SYN, 0, 0), &[], &[], 0);
    assert_eq_test!(actions.segments_len, 1, "one RST emitted");
    let rst = actions.segments[0].as_ref().unwrap();
    assert_test!((rst.flags & TCP_FLAG_RST) != 0, "RST flag");
    assert_test!(actions.release, "release");
    pass!()
}

// =============================================================================
// Plain payload accept
// =============================================================================

pub fn test_data_in_order_payload_accepted() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    let _ = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_ACK | TCP_FLAG_PSH, PEER_IRS + 1, OUR_ISS + 1),
        &[],
        b"hello",
        0,
    );
    assert_eq_test!(bufs.recv.available(), 5, "5 bytes in recv buffer");
    let d = data_ref(&pcb);
    assert_eq_test!(d.rcv_nxt.raw(), PEER_IRS + 1 + 5, "rcv_nxt advanced by 5");
    pass!()
}

pub fn test_data_in_order_payload_sets_recv_wake() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_ACK | TCP_FLAG_PSH, PEER_IRS + 1, OUR_ISS + 1),
        &[],
        b"data",
        0,
    );
    assert_test!(
        actions.notify.contains(SocketNotify::RECV_WAKE),
        "RECV_WAKE set on payload accept"
    );
    pass!()
}

// =============================================================================
// Out-of-order payload
// =============================================================================

pub fn test_data_ooo_payload_queued_and_dup_ack_emitted() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    // Gap at PEER_IRS+1..PEER_IRS+5; segment starts at PEER_IRS+5.
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_ACK, PEER_IRS + 5, OUR_ISS + 1),
        &[],
        b"worldX",
        0,
    );
    assert_eq_test!(bufs.recv.available(), 0, "nothing delivered yet");
    assert_eq_test!(actions.segments_len, 1, "duplicate ACK emitted");
    let d = data_ref(&pcb);
    assert_eq_test!(d.rcv_nxt.raw(), PEER_IRS + 1, "rcv_nxt unchanged");
    pass!()
}

// =============================================================================
// FIN transitions
// =============================================================================

pub fn test_data_fin_in_established_goes_close_wait() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_FIN | TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS + 1),
        &[],
        &[],
        0,
    );
    assert_test!(
        matches!(&pcb.state, PcbState::Data(d) if d.close_phase == ClosePhase::CloseWait),
        "transitioned to CloseWait"
    );
    assert_test!(
        actions.notify.contains(SocketNotify::PEER_CLOSED),
        "PEER_CLOSED bit"
    );
    // ACK of the FIN goes out.
    assert_eq_test!(actions.segments_len, 1, "ACK emitted");
    pass!()
}

pub fn test_data_fin_in_fin_wait_1_goes_closing() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::FinWait1);
    let mut bufs = TcpBufferPair::new();
    // Peer's FIN arrives; our FIN not yet acked.  ack_num below
    // snd_nxt means peer hasn't acked our FIN.
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_FIN | TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS),
        &[],
        &[],
        0,
    );
    assert_test!(
        matches!(&pcb.state, PcbState::Data(d) if d.close_phase == ClosePhase::Closing),
        "transitioned to Closing"
    );
    assert_eq_test!(actions.segments_len, 1, "ACK of FIN emitted");
    pass!()
}

pub fn test_data_fin_ack_in_fin_wait_1_simultaneous_close() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::FinWait1);
    let mut bufs = TcpBufferPair::new();
    // Peer FIN+ACK acks our FIN AND carries theirs → transition to
    // TimeWait directly.  snd_nxt in this test harness is OUR_ISS+1
    // so our "FIN" sits at OUR_ISS+1.
    let _actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_FIN | TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS + 1),
        &[],
        &[],
        0,
    );
    assert_test!(
        matches!(&pcb.state, PcbState::TimeWait(_)),
        "transitioned to TimeWait"
    );
    pass!()
}

pub fn test_data_fin_in_fin_wait_2_goes_time_wait() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::FinWait2);
    let mut bufs = TcpBufferPair::new();
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_FIN | TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS + 1),
        &[],
        &[],
        0,
    );
    assert_test!(
        matches!(&pcb.state, PcbState::TimeWait(_)),
        "transitioned to TimeWait"
    );
    assert_eq_test!(actions.segments_len, 1, "ACK emitted before transition");
    pass!()
}

// =============================================================================
// Pure-ACK state transitions
// =============================================================================

pub fn test_data_ack_in_fin_wait_1_transitions_to_fin_wait_2() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::FinWait1);
    let mut bufs = TcpBufferPair::new();
    // Pretend our FIN was sent at snd_nxt = OUR_ISS+1 (set by make_pcb).
    let _ = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS + 1),
        &[],
        &[],
        0,
    );
    assert_test!(
        matches!(&pcb.state, PcbState::Data(d) if d.close_phase == ClosePhase::FinWait2),
        "transitioned to FinWait2"
    );
    pass!()
}

pub fn test_data_ack_in_closing_transitions_to_time_wait() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::Closing);
    let mut bufs = TcpBufferPair::new();
    let _ = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_ACK, PEER_IRS + 2, OUR_ISS + 1),
        &[],
        &[],
        0,
    );
    assert_test!(
        matches!(&pcb.state, PcbState::TimeWait(_)),
        "transitioned to TimeWait"
    );
    pass!()
}

pub fn test_data_ack_in_last_ack_releases() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::LastAck);
    let mut bufs = TcpBufferPair::new();
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_ACK, PEER_IRS + 2, OUR_ISS + 1),
        &[],
        &[],
        0,
    );
    assert_test!(actions.release, "release set on LAST_ACK ack");
    pass!()
}

// =============================================================================
// snd_una / snd_wnd updates via process_ack
// =============================================================================

pub fn test_data_ack_advances_snd_una() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    // Bump snd_nxt so there's room for the ACK to advance.
    if let PcbState::Data(d) = &mut pcb.state {
        d.snd_nxt = SeqNum::new(OUR_ISS + 100);
        // Fake up 99 bytes in flight so retx.inflight_bytes matches
        // snd_nxt - snd_una and the invariant passes.
        let _ = d.retx.push_sent(SeqNum::new(OUR_ISS + 1), 99, 0);
    }
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS + 50),
        &[],
        &[],
        0,
    );
    let d = data_ref(&pcb);
    assert_eq_test!(d.snd_una.raw(), OUR_ISS + 50, "snd_una advanced to ack_num");
    assert_test!(
        actions.notify.contains(SocketNotify::SEND_WAKE),
        "SEND_WAKE set"
    );
    pass!()
}

// =============================================================================
// Stale ACK ignored, duplicate ACK counted
// =============================================================================

pub fn test_data_stale_ack_ignored() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    // ACK below snd_una — stale, should not change state.
    let _ = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS),
        &[],
        &[],
        0,
    );
    let d = data_ref(&pcb);
    assert_eq_test!(d.snd_una.raw(), OUR_ISS + 1, "snd_una unchanged");
    pass!()
}

pub fn test_data_duplicate_ack_counter_increments() -> TestResult {
    let mut pcb = make_pcb_in_phase(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    // Set up inflight data so dup-ack logic is reachable.
    if let PcbState::Data(d) = &mut pcb.state {
        d.snd_nxt = SeqNum::new(OUR_ISS + 100);
        let _ = d.retx.push_sent(SeqNum::new(OUR_ISS + 1), 99, 0);
    }
    let _ = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS + 1),
        &[],
        &[],
        0,
    );
    let d = data_ref(&pcb);
    assert_eq_test!(d.dup_ack_count, 1, "dup-ack counter incremented");
    pass!()
}

slopos_testing::define_test_suite!(
    tcp_pcb_data,
    [
        test_data_rst_releases_and_notifies,
        test_data_unexpected_syn_triggers_rst_and_release,
        test_data_in_order_payload_accepted,
        test_data_in_order_payload_sets_recv_wake,
        test_data_ooo_payload_queued_and_dup_ack_emitted,
        test_data_fin_in_established_goes_close_wait,
        test_data_fin_in_fin_wait_1_goes_closing,
        test_data_fin_ack_in_fin_wait_1_simultaneous_close,
        test_data_fin_in_fin_wait_2_goes_time_wait,
        test_data_ack_in_fin_wait_1_transitions_to_fin_wait_2,
        test_data_ack_in_closing_transitions_to_time_wait,
        test_data_ack_in_last_ack_releases,
        test_data_ack_advances_snd_una,
        test_data_stale_ack_ignored,
        test_data_duplicate_ack_counter_increments,
    ]
);
