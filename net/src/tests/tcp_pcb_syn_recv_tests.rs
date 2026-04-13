//! Unit tests for `tcp::pcb::syn_recv::SynRecvState::on_segment`.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::tcp::actions::SocketNotify;
use crate::tcp::header::{TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_RST, TCP_FLAG_SYN, TcpHeader};
use crate::tcp::pcb::syn_recv::SynRecvState;
use crate::tcp::pcb::{Pcb, PcbState};
use crate::tcp::seq::SeqNum;
use crate::tcp::tuple::TcpTuple;

const OUR_ISS: u32 = 10_000;
const PEER_IRS: u32 = 20_000;

fn make_pcb() -> Pcb {
    let tuple = TcpTuple {
        local_ip: [10, 0, 0, 1],
        local_port: 80,
        remote_ip: [10, 0, 0, 2],
        remote_port: 49_152,
    };
    Pcb::new(
        tuple,
        PcbState::SynRecv(SynRecvState::new(
            SeqNum::new(OUR_ISS),
            SeqNum::new(PEER_IRS),
        )),
    )
}

fn hdr(flags: u8, seq: u32, ack: u32) -> TcpHeader {
    TcpHeader {
        src_port: 49_152,
        dst_port: 80,
        seq_num: seq,
        ack_num: ack,
        data_offset: 5,
        flags,
        window_size: 32_768,
        checksum: 0,
        urgent_ptr: 0,
    }
}

/// Valid final ACK transitions SynRecv → Data.
pub fn test_syn_recv_valid_ack_transitions_to_data() -> TestResult {
    let mut pcb = make_pcb();
    let actions =
        SynRecvState::on_segment(&mut pcb, &hdr(TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS + 1), 0);
    assert_test!(
        matches!(pcb.state, PcbState::Data(_)),
        "transitioned to Data"
    );
    assert_eq_test!(actions.segments_len, 0, "no response segment");
    assert_test!(
        actions.notify.contains(SocketNotify::NEW_ESTABLISHED),
        "NEW_ESTABLISHED"
    );
    assert_test!(
        actions.notify.contains(SocketNotify::ACCEPT_WAKE),
        "ACCEPT_WAKE"
    );
    pass!()
}

/// ACK below snd_una triggers RST.
pub fn test_syn_recv_bad_low_ack_triggers_rst() -> TestResult {
    let mut pcb = make_pcb();
    // snd_una starts at iss; any ack_num < iss is out of range.
    let actions =
        SynRecvState::on_segment(&mut pcb, &hdr(TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS - 1), 0);
    assert_eq_test!(actions.segments_len, 1, "RST emitted");
    let rst = actions.segments[0].as_ref().unwrap();
    assert_test!((rst.flags & TCP_FLAG_RST) != 0, "RST flag");
    assert_test!(matches!(pcb.state, PcbState::SynRecv(_)), "state unchanged");
    pass!()
}

/// ACK above snd_nxt triggers RST.
pub fn test_syn_recv_bad_high_ack_triggers_rst() -> TestResult {
    let mut pcb = make_pcb();
    let actions = SynRecvState::on_segment(
        &mut pcb,
        &hdr(TCP_FLAG_ACK, PEER_IRS + 1, OUR_ISS + 5_000),
        0,
    );
    assert_eq_test!(actions.segments_len, 1, "RST emitted");
    pass!()
}

/// RST releases the PCB and sets RESET_RECEIVED (RFC 5961: must be in window).
pub fn test_syn_recv_rst_releases_pcb() -> TestResult {
    let mut pcb = make_pcb();
    // rcv_nxt = PEER_IRS + 1 after handshake; RST must be in-window.
    let actions = SynRecvState::on_segment(&mut pcb, &hdr(TCP_FLAG_RST, PEER_IRS + 1, 0), 0);
    assert_test!(actions.release, "release");
    assert_test!(
        actions.notify.contains(SocketNotify::RESET_RECEIVED),
        "RESET_RECEIVED"
    );
    pass!()
}

/// Non-ACK segment (bare SYN) is dropped silently.
pub fn test_syn_recv_bare_syn_dropped() -> TestResult {
    let mut pcb = make_pcb();
    let actions = SynRecvState::on_segment(&mut pcb, &hdr(TCP_FLAG_SYN, 0, 0), 0);
    assert_eq_test!(actions.segments_len, 0, "no response");
    assert_test!(matches!(pcb.state, PcbState::SynRecv(_)), "state unchanged");
    pass!()
}

/// Bare FIN is likewise dropped.
pub fn test_syn_recv_bare_fin_dropped() -> TestResult {
    let mut pcb = make_pcb();
    let actions = SynRecvState::on_segment(&mut pcb, &hdr(TCP_FLAG_FIN, 0, 0), 0);
    assert_eq_test!(actions.segments_len, 0, "no response");
    pass!()
}

slopos_testing::define_test_suite!(
    tcp_pcb_syn_recv,
    [
        test_syn_recv_valid_ack_transitions_to_data,
        test_syn_recv_bad_low_ack_triggers_rst,
        test_syn_recv_bad_high_ack_triggers_rst,
        test_syn_recv_rst_releases_pcb,
        test_syn_recv_bare_syn_dropped,
        test_syn_recv_bare_fin_dropped,
    ]
);
