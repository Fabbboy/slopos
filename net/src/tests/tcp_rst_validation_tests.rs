//! RFC 5961 RST validation and challenge ACK tests.

use slopos_testing::{TestResult, assert_eq_test, assert_test, pass};

use crate::tcp::actions::SocketNotify;
use crate::tcp::buffer::{TCP_BUFFER_SIZE, TcpBufferPair};
use crate::tcp::challenge_ack::{self, RstAction};
use crate::tcp::header::{TCP_FLAG_ACK, TCP_FLAG_RST, TCP_FLAG_SYN, TcpHeader};
use crate::tcp::pcb::data::{ClosePhase, DataState};
use crate::tcp::pcb::syn_recv::SynRecvState;
use crate::tcp::pcb::time_wait::TimeWaitState;
use crate::tcp::pcb::{Pcb, PcbState};
use crate::tcp::seq::SeqNum;
use crate::tcp::tuple::TcpTuple;
use crate::tests::tcp_common::{LOCAL_IP, REMOTE_IP};

const LOCAL_PORT: u16 = 49_152;
const REMOTE_PORT: u16 = 80;

const OUR_ISS: u32 = 10_000;
const PEER_IRS: u32 = 20_000;

const LAST_RCV_NXT: u32 = 12_345;
const LAST_SND_NXT: u32 = 67_890;

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
    let mut data: slopos_ostd::KBox<DataState> = slopos_ostd::KBox::try_init(DataState::init_new(
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
    ))
    .expect("alloc");
    data.close_phase = phase;
    if matches!(
        phase,
        ClosePhase::CloseWait | ClosePhase::Closing | ClosePhase::LastAck
    ) {
        data.peer_closed = true;
    }
    Pcb::new(tuple(), PcbState::Data(data))
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
    assert_eq_test!(
        challenge_ack::classify_rst(0x0000_0005, rcv_nxt, window),
        RstAction::ChallengeAck,
        "wrapped in-window"
    );
    assert_eq_test!(
        challenge_ack::classify_rst(0x8000_0000, rcv_nxt, window),
        RstAction::Drop,
        "wrapped out-of-window"
    );
    assert_eq_test!(
        challenge_ack::classify_rst(rcv_nxt, rcv_nxt, window),
        RstAction::Accept,
        "exact at wrap"
    );
    pass!()
}

pub fn test_data_rst_exact_seq_tears_down() -> TestResult {
    let mut pcb = make_data_pcb(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new(TCP_BUFFER_SIZE).expect("alloc");
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
    let mut pcb = make_data_pcb(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new(TCP_BUFFER_SIZE).expect("alloc");
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
    let mut bufs = TcpBufferPair::new(TCP_BUFFER_SIZE).expect("alloc");
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
        let mut bufs = TcpBufferPair::new(TCP_BUFFER_SIZE).expect("alloc");
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
        let mut bufs = TcpBufferPair::new(TCP_BUFFER_SIZE).expect("alloc");
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
    let outside_seq = (PEER_IRS + 1).wrapping_add(50_000);
    let actions = SynRecvState::on_segment(&mut pcb, &hdr(TCP_FLAG_RST, outside_seq, 0), 0);
    assert_test!(!actions.release, "out-of-window RST does not release");
    assert_eq_test!(actions.segments_len, 0, "no segments emitted");
    assert_test!(matches!(pcb.state, PcbState::SynRecv(_)), "still SynRecv");
    pass!()
}

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

pub fn test_challenge_ack_rate_limit() -> TestResult {
    let mut budget = challenge_ack::ChallengeBudget::new();
    // The cap is jittered into [LIMIT/2, LIMIT), so the guarantee under test
    // is that the budget is finite and that the first half is always granted.
    for _ in 0..500 {
        assert_test!(budget.try_consume(1000), "within the guaranteed floor");
    }
    let mut exhausted = false;
    for _ in 0..1000 {
        if !budget.try_consume(1000) {
            exhausted = true;
            break;
        }
    }
    assert_test!(exhausted, "budget is finite within one epoch");
    pass!()
}

pub fn test_challenge_ack_rate_resets_after_epoch() -> TestResult {
    let mut budget = challenge_ack::ChallengeBudget::new();
    let mut exhausted = false;
    for _ in 0..1500 {
        if !budget.try_consume(1000) {
            exhausted = true;
            break;
        }
    }
    assert_test!(exhausted, "exhausted at t=1000");
    assert_test!(budget.try_consume(2001), "allowed in new epoch");
    pass!()
}

/// RFC 5961 §4: a blind SYN on an established connection must be answered
/// with a challenge ACK, never a RST, and must not release the PCB.
pub fn test_blind_syn_does_not_tear_down_connection() -> TestResult {
    let mut pcb = make_data_pcb(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new(TCP_BUFFER_SIZE).expect("alloc");
    let off_window_seq = (PEER_IRS + 1).wrapping_add(50_000);
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_SYN, off_window_seq, 0),
        &[],
        &[],
        1,
    );
    assert_test!(!actions.release, "a blind SYN must not release the PCB");
    assert_test!(
        !actions.notify.contains(SocketNotify::RESET_RECEIVED),
        "a blind SYN must not report a reset"
    );
    assert_eq_test!(actions.segments_len, 1, "challenge ACK emitted");
    let seg = actions.segments[0].as_ref().unwrap();
    assert_test!((seg.flags & TCP_FLAG_ACK) != 0, "ACK flag");
    assert_test!((seg.flags & TCP_FLAG_RST) == 0, "never a RST");
    assert_eq_test!(seg.ack_num, PEER_IRS + 1, "ack_num == rcv_nxt");
    pass!()
}

/// RFC 793 §3.9: data outside the receive window is dropped with an ACK, not
/// delivered to the socket.
pub fn test_out_of_window_data_is_not_accepted() -> TestResult {
    let mut pcb = make_data_pcb(ClosePhase::Established);
    let mut bufs = TcpBufferPair::new(TCP_BUFFER_SIZE).expect("alloc");
    let off_window_seq = (PEER_IRS + 1).wrapping_add(50_000);
    let payload = [0xAAu8; 4];
    let actions = DataState::on_segment(
        &mut pcb,
        &mut bufs,
        &hdr(TCP_FLAG_ACK, off_window_seq, OUR_ISS + 1),
        &[],
        &payload,
        1,
    );
    assert_test!(!actions.release, "no release");
    assert_eq_test!(
        bufs.recv.available(),
        0,
        "out-of-window data must not be queued"
    );
    assert_eq_test!(actions.segments_len, 1, "a resync ACK is sent");
    pass!()
}

pub fn test_segment_acceptable_table() -> TestResult {
    use challenge_ack::segment_acceptable;

    assert_test!(
        segment_acceptable(100, 0, 100, 0),
        "zero-length at rcv_nxt with a closed window"
    );
    assert_test!(
        !segment_acceptable(101, 0, 100, 0),
        "zero-length off rcv_nxt with a closed window"
    );
    assert_test!(
        segment_acceptable(100, 0, 100, 1000),
        "zero-length in window"
    );
    assert_test!(
        !segment_acceptable(2000, 0, 100, 1000),
        "zero-length past the window"
    );
    assert_test!(
        !segment_acceptable(100, 4, 100, 0),
        "data is never acceptable into a closed window"
    );
    assert_test!(segment_acceptable(100, 4, 100, 1000), "data in window");
    assert_test!(
        segment_acceptable(98, 4, 100, 1000),
        "a segment straddling the left edge is acceptable"
    );
    assert_test!(
        !segment_acceptable(5000, 4, 100, 1000),
        "data wholly past the window"
    );

    // The window wraps the 32-bit sequence line.
    let near_wrap: u32 = 0xFFFF_FFF0;
    assert_test!(
        segment_acceptable(0x0000_0004, 0, near_wrap, 32),
        "wrapped in-window"
    );
    assert_test!(
        !segment_acceptable(0x8000_0000, 0, near_wrap, 32),
        "wrapped out-of-window"
    );
    pass!()
}

/// One connection's budget must not report on another's: the CVE-2016-5696
/// side channel is exactly a shared counter.
pub fn test_challenge_ack_budget_is_per_connection() -> TestResult {
    let mut victim = challenge_ack::ChallengeBudget::new();
    let mut attacker = challenge_ack::ChallengeBudget::new();

    let mut drained = false;
    for _ in 0..1500 {
        if !victim.try_consume(1000) {
            drained = true;
            break;
        }
    }
    assert_test!(drained, "victim budget drained");
    assert_test!(
        attacker.try_consume(1000),
        "a drained peer must not deny this connection its own budget"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_classify_rst_exact_match,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_classify_rst_in_window,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_classify_rst_outside_window,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_classify_rst_zero_window,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_classify_rst_wrapping_seq,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_data_rst_exact_seq_tears_down,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_data_rst_in_window_sends_challenge_ack,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_data_rst_outside_window_dropped,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_data_rst_challenge_ack_each_close_phase,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_data_rst_exact_seq_each_close_phase,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_syn_recv_rst_in_window_releases,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_syn_recv_rst_outside_window_dropped,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_time_wait_rst_in_window_releases,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_time_wait_rst_outside_window_dropped,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_challenge_ack_rate_limit,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_challenge_ack_rate_resets_after_epoch,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_challenge_ack_budget_is_per_connection,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_blind_syn_does_not_tear_down_connection,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_out_of_window_data_is_not_accepted,
    suite = tcp_rst_validation
);
slopos_testing::stest!(
    name = test_segment_acceptable_table,
    suite = tcp_rst_validation
);
