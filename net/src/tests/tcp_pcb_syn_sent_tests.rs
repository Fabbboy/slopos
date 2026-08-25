//! Unit tests for `tcp::pcb::syn_sent::SynSentState::on_segment`.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::tcp::actions::SocketNotify;
use crate::tcp::header::{TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_RST, TCP_FLAG_SYN, TcpHeader};
use crate::tcp::pcb::syn_sent::SynSentState;
use crate::tcp::pcb::{Pcb, PcbState};
use crate::tcp::seq::SeqNum;
use crate::tcp::tuple::TcpTuple;
use crate::tests::tcp_common::{LOCAL_IP, REMOTE_IP};

const LOCAL_PORT: u16 = 49152;
const REMOTE_PORT: u16 = 80;
const OUR_ISS: u32 = 10_000;

fn make_pcb() -> Pcb {
    let tuple = TcpTuple {
        local_ip: LOCAL_IP,
        local_port: LOCAL_PORT,
        remote_ip: REMOTE_IP,
        remote_port: REMOTE_PORT,
    };
    Pcb::new(
        tuple,
        PcbState::SynSent(SynSentState::new(SeqNum::new(OUR_ISS))),
    )
}

fn hdr(flags: u8, seq: u32, ack: u32) -> TcpHeader {
    TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: LOCAL_PORT,
        seq_num: seq,
        ack_num: ack,
        data_offset: 5,
        flags,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    }
}

pub fn test_syn_sent_syn_ack_transitions_to_data() -> TestResult {
    let mut pcb = make_pcb();
    let peer_iss = 5_000u32;
    let actions = SynSentState::on_segment(
        &mut pcb,
        &hdr(TCP_FLAG_SYN | TCP_FLAG_ACK, peer_iss, OUR_ISS + 1),
        &[],
        0,
    );
    assert_test!(
        matches!(pcb.state, PcbState::Data(_)),
        "transitioned to Data"
    );
    assert_eq_test!(actions.segments_len, 1, "one ACK emitted");
    let ack = actions.segments[0].as_ref().unwrap();
    assert_test!((ack.flags & TCP_FLAG_ACK) != 0, "response has ACK flag");
    assert_test!((ack.flags & TCP_FLAG_SYN) == 0, "response is NOT SYN+ACK");
    assert_eq_test!(ack.ack_num, peer_iss + 1, "acks peer ISS + 1");
    assert_test!(
        actions.notify.contains(SocketNotify::NEW_ESTABLISHED),
        "NEW_ESTABLISHED notify set"
    );
    pass!()
}

pub fn test_syn_sent_simultaneous_open() -> TestResult {
    let mut pcb = make_pcb();
    let actions = SynSentState::on_segment(&mut pcb, &hdr(TCP_FLAG_SYN, 3000, 0), &[], 0);
    assert_test!(
        matches!(pcb.state, PcbState::SynRecv(_)),
        "transitioned to SynRecv"
    );
    assert_eq_test!(actions.segments_len, 1, "one SYN+ACK emitted");
    let seg = actions.segments[0].as_ref().unwrap();
    assert_eq_test!(
        seg.flags & (TCP_FLAG_SYN | TCP_FLAG_ACK),
        TCP_FLAG_SYN | TCP_FLAG_ACK,
        "SYN+ACK flags"
    );
    pass!()
}

pub fn test_syn_sent_rst_ack_refused() -> TestResult {
    let mut pcb = make_pcb();
    let actions = SynSentState::on_segment(
        &mut pcb,
        &hdr(TCP_FLAG_RST | TCP_FLAG_ACK, 0, OUR_ISS + 1),
        &[],
        0,
    );
    assert_test!(actions.release, "PCB marked for release");
    assert_test!(
        actions.notify.contains(SocketNotify::RESET_RECEIVED),
        "RESET_RECEIVED bit"
    );
    assert_eq_test!(actions.segments_len, 0, "no response segment");
    pass!()
}

pub fn test_syn_sent_bare_rst_ignored() -> TestResult {
    let mut pcb = make_pcb();
    let actions = SynSentState::on_segment(&mut pcb, &hdr(TCP_FLAG_RST, 0, 0), &[], 0);
    assert_eq_test!(actions.segments_len, 0, "no segments");
    assert_test!(!actions.release, "not released");
    assert_test!(matches!(pcb.state, PcbState::SynSent(_)), "state unchanged");
    pass!()
}

pub fn test_syn_sent_bad_ack_triggers_rst() -> TestResult {
    let mut pcb = make_pcb();
    let actions = SynSentState::on_segment(&mut pcb, &hdr(TCP_FLAG_ACK, 0, OUR_ISS - 1), &[], 0);
    assert_eq_test!(actions.segments_len, 1, "one RST emitted");
    let rst = actions.segments[0].as_ref().unwrap();
    assert_test!((rst.flags & TCP_FLAG_RST) != 0, "response is RST");
    assert_eq_test!(rst.seq_num, OUR_ISS - 1, "seq = bad ack_num");
    assert_test!(
        matches!(pcb.state, PcbState::SynSent(_)),
        "state unchanged after bad ACK"
    );
    pass!()
}

pub fn test_syn_sent_bare_fin_ignored() -> TestResult {
    let mut pcb = make_pcb();
    let actions = SynSentState::on_segment(&mut pcb, &hdr(TCP_FLAG_FIN, 0, 0), &[], 0);
    assert_eq_test!(actions.segments_len, 0, "no segments");
    pass!()
}

pub fn test_syn_sent_parses_mss_from_syn_ack() -> TestResult {
    let mut pcb = make_pcb();
    // MSS = 1200
    let opts = [2, 4, 0x04, 0xB0];
    let _ = SynSentState::on_segment(
        &mut pcb,
        &hdr(TCP_FLAG_SYN | TCP_FLAG_ACK, 5000, OUR_ISS + 1),
        &opts,
        0,
    );
    if let PcbState::Data(d) = &pcb.state {
        assert_eq_test!(d.peer_mss, 1200, "DataState.peer_mss");
    } else {
        return slopos_testing::fail!("expected Data state");
    }
    pass!()
}

pub fn test_syn_sent_parses_wscale() -> TestResult {
    let mut pcb = make_pcb();
    // Window Scale = 3 (NOP + WScale to align to 4 bytes)
    let opts = [1, 3, 3, 3];
    let _ = SynSentState::on_segment(
        &mut pcb,
        &hdr(TCP_FLAG_SYN | TCP_FLAG_ACK, 5000, OUR_ISS + 1),
        &opts,
        0,
    );
    if let PcbState::Data(d) = &pcb.state {
        assert_eq_test!(d.snd_wscale, 3, "negotiated peer wscale");
        assert_test!(d.wscale_enabled, "wscale_enabled");
    } else {
        return slopos_testing::fail!("expected Data state");
    }
    pass!()
}

slopos_testing::stest!(
    name = test_syn_sent_syn_ack_transitions_to_data,
    suite = tcp_pcb_syn_sent
);
slopos_testing::stest!(
    name = test_syn_sent_simultaneous_open,
    suite = tcp_pcb_syn_sent
);
slopos_testing::stest!(
    name = test_syn_sent_rst_ack_refused,
    suite = tcp_pcb_syn_sent
);
slopos_testing::stest!(
    name = test_syn_sent_bare_rst_ignored,
    suite = tcp_pcb_syn_sent
);
slopos_testing::stest!(
    name = test_syn_sent_bad_ack_triggers_rst,
    suite = tcp_pcb_syn_sent
);
slopos_testing::stest!(
    name = test_syn_sent_bare_fin_ignored,
    suite = tcp_pcb_syn_sent
);
slopos_testing::stest!(
    name = test_syn_sent_parses_mss_from_syn_ack,
    suite = tcp_pcb_syn_sent
);
slopos_testing::stest!(name = test_syn_sent_parses_wscale, suite = tcp_pcb_syn_sent);
