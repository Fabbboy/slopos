//! RFC 5961 RST validation and challenge ACK tests.
//!
//! Covers: DataState (all 6 close phases), SynRecv, TimeWait, rate limiter,
//! wrapping sequence arithmetic, and zero-window edge case.

use slopos_testing::{TestResult, assert_eq_test, assert_test, pass};

use crate::tcp::actions::SocketNotify;
use crate::tcp::buffer::TcpBufferPair;
use crate::tcp::challenge_ack::{self, RstAction};
use crate::tcp::header::{TCP_FLAG_ACK, TCP_FLAG_RST, TcpHeader};
use crate::tcp::pcb::data::{ClosePhase, DataState};
use crate::tcp::pcb::syn_recv::SynRecvState;
use crate::tcp::pcb::time_wait::TimeWaitState;
use crate::tcp::pcb::{Pcb, PcbState};
use crate::tcp::seq::SeqNum;
use crate::tcp::tuple::TcpTuple;

// =============================================================================
// Constants
// =============================================================================

const LOCAL_IP: [u8; 4] = [10, 0, 0, 1];
const REMOTE_IP: [u8; 4] = [10, 0, 0, 2];
const LOCAL_PORT: u16 = 49_152;
const REMOTE_PORT: u16 = 80;

const OUR_ISS: u32 = 10_000;
const PEER_IRS: u32 = 20_000;

const LAST_RCV_NXT: u32 = 12_345;
const LAST_SND_NXT: u32 = 67_890;

// =============================================================================
// Helpers
// =============================================================================

fn tuple() -> TcpTuple {
    TcpTuple {
        local_ip: LOCAL_IP,
        local_port: LOCAL_PORT,
        remote_ip: REMOTE_IP,
        remote_port: REMOTE_PORT,
    }
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

fn make_data_pcb(phase: ClosePhase) -> Pcb {
    let mut data = DataState::new(
        SeqNum::new(OUR_ISS),
        SeqNum::new(PEER_IRS),
        SeqNum::new(OUR_ISS + 1),
        SeqNum::new(OUR_ISS + 1),
        SeqNum::new(PEER_IRS + 1),
        65_535,
        32_768,
        1460,
        0,
        0,
        false,
        false,
    );
    data.close_phase = phase;
    if matches!(
        phase,
        ClosePhase::CloseWait | ClosePhase::Closing | ClosePhase::LastAck
    ) {
        data.peer_closed = true;
    }
    Pcb::new(tuple(), PcbState::Data(alloc::boxed::Box::new(data)))
}

fn make_syn_recv_pcb() -> Pcb {
    Pcb::new(
        tuple(),
        PcbState::SynRecv(SynRecvState::new(
            SeqNum::new(OUR_ISS),
            SeqNum::new(PEER_IRS),
        )),
    )
}

fn make_time_wait_pcb() -> Pcb {
    Pcb::new(
        tuple(),
        PcbState::TimeWait(TimeWaitState::new(
            SeqNum::new(LAST_RCV_NXT),
            SeqNum::new(LAST_SND_NXT),
            32_768,
            1000,
        )),
    )
}

// =============================================================================
// classify_rst unit tests
// =============================================================================

pub fn test_classify_rst_exact_match() -> TestResult {
    assert_eq_test!(
        challenge_ack::classify_rst(100, 100, 32768),
        RstAction::Accept,
        "exact match"
    );
    pass!()
}

pub fn test_classify_rst_in_window() -> TestResult {
    assert_eq_test!(
        challenge_ack::classify_rst(200, 100, 32768),
        RstAction::ChallengeAck,
        "in window"
    );
    pass!()
}

pub fn test_classify_rst_outside_window() -> TestResult {
    assert_eq_test!(
        challenge_ack::classify_rst(40000, 100, 32768),
        RstAction::Drop,
        "outside window"
    );
    pass!()
}

pub fn test_classify_rst_zero_window() -> TestResult {
    assert_eq_test!(
        challenge_ack::classify_rst(100, 100, 0),
        RstAction::Accept,
        "zero window exact"
    );
    assert_eq_test!(
        challenge_ack::classify_rst(101, 100, 0),
        RstAction::Drop,
        "zero window non-exact"
    );
    pass!()
}

pub fn test_classify_rst_wrapping_seq() -> TestResult {
    // rcv_nxt near wrap point, window spans across 0.
    let rcv_nxt: u32 = 0xFFFF_FFF0;
    let window: u32 = 32;
    // seq just past wrap — should be in window.
    assert_eq_test!(
        challenge_ack::classify_rst(0x0000_0005, rcv_nxt, window),
        RstAction::ChallengeAck,
        "wrapped in-window"
    );
    // seq far before — outside window.
    assert_eq_test!(
        challenge_ack::classify_rst(0x8000_0000, rcv_nxt, window),
        RstAction::Drop,
        "wrapped out-of-window"
    );
    // exact match at wrap point.
    assert_eq_test!(
        challenge_ack::classify_rst(rcv_nxt, rcv_nxt, window),
        RstAction::Accept,
        "exact at wrap"
    );
    pass!()
}

// =============================================================================
// DataState RST tests
// =============================================================================

pub fn test_data_rst_exact_seq_tears_down() -> TestResult {
    let mut pcb = make_data_pcb(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    let rcv_nxt = PEER_IRS + 1;
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_RST, rcv_nxt, 0),
        &[],
        &[],
        0,
    );
    assert_test!(actions.release, "release set on exact RST");
    assert_test!(
        actions.notify.contains(SocketNotify::RESET_RECEIVED),
        "RESET_RECEIVED"
    );
    pass!()
}

pub fn test_data_rst_in_window_sends_challenge_ack() -> TestResult {
    challenge_ack::reset_for_tests();
    let mut pcb = make_data_pcb(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    // In-window but not exact: rcv_nxt + 100.
    let in_window_seq = (PEER_IRS + 1).wrapping_add(100);
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_RST, in_window_seq, 0),
        &[],
        &[],
        1,
    );
    assert_test!(!actions.release, "no release on in-window RST");
    assert_eq_test!(actions.segments_len, 1, "challenge ACK emitted");
    let seg = actions.segments[0].as_ref().unwrap();
    assert_test!((seg.flags & TCP_FLAG_ACK) != 0, "ACK flag");
    assert_test!((seg.flags & TCP_FLAG_RST) == 0, "not RST");
    assert_eq_test!(seg.ack_num, PEER_IRS + 1, "ack_num == rcv_nxt");
    assert_eq_test!(seg.seq_num, OUR_ISS + 1, "seq_num == snd_nxt");
    pass!()
}

pub fn test_data_rst_outside_window_dropped() -> TestResult {
    let mut pcb = make_data_pcb(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new();
    // Far outside window.
    let outside_seq = (PEER_IRS + 1).wrapping_add(50_000);
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_RST, outside_seq, 0),
        &[],
        &[],
        0,
    );
    assert_test!(!actions.release, "no release");
    assert_eq_test!(actions.segments_len, 0, "no segments emitted");
    pass!()
}

pub fn test_data_rst_challenge_ack_each_close_phase() -> TestResult {
    challenge_ack::reset_for_tests();
    let phases = [
        ClosePhase::Established,
        ClosePhase::FinWait1,
        ClosePhase::FinWait2,
        ClosePhase::CloseWait,
        ClosePhase::Closing,
        ClosePhase::LastAck,
    ];
    let in_window_seq = (PEER_IRS + 1).wrapping_add(100);
    for (i, phase) in phases.iter().enumerate() {
        let mut pcb = make_data_pcb(*phase);
        let mut bufs = TcpBufferPair::new();
        let actions = DataState::on_segment(
            &mut pcb,
            &mut bufs,
            &hdr(TCP_FLAG_RST, in_window_seq, 0),
            &[],
            &[],
            1,
        );
        assert_test!(!actions.release, "no release in close phase");
        assert_test!(actions.segments_len >= 1, "challenge ACK in close phase");
        let _ = i;
    }
    pass!()
}

pub fn test_data_rst_exact_seq_each_close_phase() -> TestResult {
    let phases = [
        ClosePhase::Established,
        ClosePhase::FinWait1,
        ClosePhase::FinWait2,
        ClosePhase::CloseWait,
        ClosePhase::Closing,
        ClosePhase::LastAck,
    ];
    let rcv_nxt = PEER_IRS + 1;
    for (i, phase) in phases.iter().enumerate() {
        let mut pcb = make_data_pcb(*phase);
        let mut bufs = TcpBufferPair::new();
        let actions = DataState::on_segment(
            &mut pcb,
            &mut bufs,
            &hdr(TCP_FLAG_RST, rcv_nxt, 0),
            &[],
            &[],
            0,
        );
        assert_test!(actions.release, "release in close phase");
        let _ = i;
    }
    pass!()
}

// =============================================================================
// SynRecv RST tests
// =============================================================================

pub fn test_syn_recv_rst_in_window_releases() -> TestResult {
    let mut pcb = make_syn_recv_pcb();
    let rcv_nxt = PEER_IRS + 1;
    let actions = SynRecvState::on_segment(&mut pcb, &hdr(TCP_FLAG_RST, rcv_nxt, 0), 0);
    assert_test!(actions.release, "in-window RST releases SynRecv");
    assert_test!(
        actions.notify.contains(SocketNotify::RESET_RECEIVED),
        "RESET_RECEIVED"
    );
    pass!()
}

pub fn test_syn_recv_rst_outside_window_dropped() -> TestResult {
    let mut pcb = make_syn_recv_pcb();
    // Sequence far outside the initial window.
    let outside_seq = (PEER_IRS + 1).wrapping_add(50_000);
    let actions = SynRecvState::on_segment(&mut pcb, &hdr(TCP_FLAG_RST, outside_seq, 0), 0);
    assert_test!(!actions.release, "out-of-window RST does not release");
    assert_eq_test!(actions.segments_len, 0, "no segments emitted");
    // PCB should still be in SynRecv.
    assert_test!(matches!(pcb.state, PcbState::SynRecv(_)), "still SynRecv");
    pass!()
}

// =============================================================================
// TimeWait RST tests
// =============================================================================

pub fn test_time_wait_rst_in_window_releases() -> TestResult {
    let mut pcb = make_time_wait_pcb();
    let actions = TimeWaitState::on_segment(&mut pcb, &hdr(TCP_FLAG_RST, LAST_RCV_NXT, 0), 2000);
    assert_test!(actions.release, "in-window RST releases TimeWait");
    pass!()
}

pub fn test_time_wait_rst_outside_window_dropped() -> TestResult {
    let mut pcb = make_time_wait_pcb();
    let outside_seq = LAST_RCV_NXT.wrapping_add(50_000);
    let actions = TimeWaitState::on_segment(&mut pcb, &hdr(TCP_FLAG_RST, outside_seq, 0), 2000);
    assert_test!(!actions.release, "out-of-window RST does not release");
    assert_eq_test!(actions.segments_len, 0, "no segments emitted");
    pass!()
}

// =============================================================================
// Challenge ACK rate limiter tests
// =============================================================================

pub fn test_challenge_ack_rate_limit() -> TestResult {
    challenge_ack::reset_for_tests();
    // Exhaust the limit.
    for _ in 0..1000 {
        assert_test!(challenge_ack::try_challenge_ack(1000), "within limit");
    }
    // 1001st should be rate-limited.
    assert_test!(!challenge_ack::try_challenge_ack(1000), "1001st blocked");
    pass!()
}

pub fn test_challenge_ack_rate_resets_after_epoch() -> TestResult {
    challenge_ack::reset_for_tests();
    // Exhaust at t=1000.
    for _ in 0..1000 {
        challenge_ack::try_challenge_ack(1000);
    }
    assert_test!(
        !challenge_ack::try_challenge_ack(1000),
        "exhausted at t=1000"
    );
    // New epoch at t=2001.
    assert_test!(
        challenge_ack::try_challenge_ack(2001),
        "allowed in new epoch"
    );
    pass!()
}

// =============================================================================
// Test suite registration
// =============================================================================

slopos_testing::define_test_suite!(
    tcp_rst_validation,
    [
        // classify_rst unit tests
        test_classify_rst_exact_match,
        test_classify_rst_in_window,
        test_classify_rst_outside_window,
        test_classify_rst_zero_window,
        test_classify_rst_wrapping_seq,
        // DataState RST
        test_data_rst_exact_seq_tears_down,
        test_data_rst_in_window_sends_challenge_ack,
        test_data_rst_outside_window_dropped,
        test_data_rst_challenge_ack_each_close_phase,
        test_data_rst_exact_seq_each_close_phase,
        // SynRecv RST
        test_syn_recv_rst_in_window_releases,
        test_syn_recv_rst_outside_window_dropped,
        // TimeWait RST
        test_time_wait_rst_in_window_releases,
        test_time_wait_rst_outside_window_dropped,
        // Rate limiter
        test_challenge_ack_rate_limit,
        test_challenge_ack_rate_resets_after_epoch,
    ]
);
